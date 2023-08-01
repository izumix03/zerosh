use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::CString;
use std::os::fd::RawFd;
use std::process::exit;
use std::sync::mpsc::{channel, Sender, sync_channel, SyncSender};
use std::thread;

use nix::{libc, unistd};
use nix::libc::{SIGCHLD, SIGINT, SIGTSTP};
use nix::sys::signal::{killpg, SigHandler, Signal, signal};
use nix::unistd::{dup2, execvp, fork, ForkResult, Pid, pipe, setpgid, tcgetpgrp, tcsetpgrp};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use signal_hook::iterator::Signals;

use crate::helper::DynError;

/// システムコール呼び出しのラッパ
/// EINTR(システム割り込み) ならリトライ
///
/// f:  システムコールを呼び出す関数
fn syscall<F, T>(f: F) -> Result<T, nix::Error>
    where
        F: Fn() -> Result<T, nix::Error>,
{
    loop {
        match f() {
            Err(nix::Error::EINTR) => (), // リトライ
            result => return result,
        }
    }
}

/// worker スレッドが受信するメッセージ
enum WorkerMsg {
    Signal(i32),
    // シグナルを受信
    Cmd(String), // コマンド入力
}

/// main スレッドが受信するメッセージ
enum ShellMsg {
    Continue(i32),
    // シェルの
    Quit(i32),
}

#[derive(Debug)]
pub struct Shell {
    logfile: String, // ログファイル
}

impl Shell {
    pub fn new(logfile: &str) -> Self {
        Shell {
            logfile: logfile.to_string(),
        }
    }

    // mainスレッド
    pub fn run(&self) -> Result<(), DynError> {
        // SIGTTOU を無視に設定しないと、SIGTSTPが配送される
        // SIGTTOU -> SigIgn に変換してシェルの停止を無視
        unsafe { signal(Signal::SIGTTOU, SigHandler::SigIgn).unwrap() };

        // 最新版ではHistoryが必要、自分で定義しても良いけど下記でもOK
        // 標準週力の読み込みを行う、矢印キーなどもサポート
        let mut rl = DefaultEditor::new()?;

        // ？ ヒストリの読み込み、結果はどうなるの？
        if let Err(e) = rl.load_history(&self.logfile) {
            eprintln!("ZeroSh: ヒストリファイルの読み込みに失敗: {e}");
        }

        // チャネルを生成して、signal_handler と worker スレッドを生成
        // 非同期チャネル
        let (worker_tx, worker_rx) = channel();
        // 同期チャネル: sync_channel でバッファサイズ0
        let (shell_tx, shell_rx) = sync_channel(0);

        // 起動
        spawn_sig_handler(worker_tx.clone())?;
        // 起動
        // Worker::new().spawn(worker_rx, shell_tx);

        // 終了コード
        let exit_val;
        // 直前に終了したぷろせす の終了コード
        let mut prev = 0;

        loop {
            // 1行読み込んで、 worker に送信
            // ニコニコマーク or ドクロマーク
            let face = if prev == 0 { '\u{1F642}' } else { '\u{1F480}' };
            // 待ち受けている時に ZeroSh ?? %> って出る。?? は上のやつ
            match rl.readline(&format!("ZeroSh {face} %>")) {
                // メイン処理
                Ok(line) => {
                    let line_trimmed = line.trim();
                    if line_trimmed.is_empty() {
                        continue; // 空コマンドの場合は再読み込み
                    } else {
                        // ヒストリファイルへ追加、エラーしても無視
                        let _ = rl.add_history_entry(line_trimmed);
                    }

                    // Worker に送信
                    worker_tx.send(WorkerMsg::Cmd(line)).unwrap();
                    match shell_rx.recv().unwrap() {
                        ShellMsg::Continue(n) => {
                            // 読み込み再開, まだ読んでないけど continue とかいらない？
                            prev = n
                        }
                        ShellMsg::Quit(n) => {
                            // 終了
                            exit_val = n;
                            break;
                        }
                    }
                }
                Err(ReadlineError::Interrupted) => eprintln!("ZeroSh: 終了はCtrl + D"),
                Err(ReadlineError::Eof) => { // Cmd + d
                    worker_tx.send(WorkerMsg::Cmd("exit".to_string())).unwrap();
                    match shell_rx.recv().unwrap() {
                        ShellMsg::Quit(n) => {
                            // シェル終了
                            exit_val = n;
                            break;
                        }
                        _ => panic!("exitに失敗"),
                    }
                }
                Err(e) => {
                    eprintln!("Zerosh: 組み込みエラー\n{e}");
                    exit_val = 1;
                    break;
                }
            }
        }
        if let Err(e) = rl.save_history(&self.logfile) {
            eprintln!("Zerosh: ヒストリファイルへの書き込み失敗: {e}");
        }
        exit(exit_val)
    }
}

fn spawn_sig_handler(tx: Sender<WorkerMsg>) -> Result<(), DynError> {
    // シグナル受信
    let mut signals = Signals::new(&[SIGINT, SIGTSTP, SIGCHLD])?;
    thread::spawn(move || {
        // シグナル受信をずっと待ち続けるイテレータ
        for sig in signals.forever() {
            tx.send(WorkerMsg::Signal(sig)).unwrap()
        }
    });

    Ok(())
}

// プロセス情報と worker スレッド
#[derive(Debug, PartialEq, Eq, Clone)]
enum ProcState {
    Run,
    Stop,
}

#[derive(Debug, Clone)]
struct ProcInfo {
    state: ProcState,
    pgid: Pid, // プロセスグループID
}

#[derive(Debug)]
struct Worker {
    exit_val: i32,
    fg: Option<Pid>, // フォアグラウンドのプロセスグループID

    // ジョブIDから (プロセスグループID、実行コマンド) へのマップ
    jobs: BTreeMap<usize, (Pid, String)>,

    // プロセスグループIDから (ジョブID,プロセスID)へのマップ
    pgid_to_pid: HashMap<Pid, (usize, HashSet<Pid>)>,

    pid_to_info: HashMap<Pid, ProcInfo>,
    // プロセスIDから プロセスグループIDへのマップ
    shell_pgid: Pid, // シェルのプロセスグループID
}

impl Worker {
    fn new() -> Self {
        Worker {
            exit_val: 0,
            fg: None, // フォアグラウンドはシェル
            jobs: BTreeMap::new(),
            pgid_to_pid: HashMap::new(),
            pid_to_info: HashMap::new(),
            // シェルのプロセスグループIDを取得
            // ファイルディスクリプタに関連付けられたフォアグラウンドのプロセスグループIDを取得する
            // libc::STDIN_FILENO は標準入力
            // ちなみに、getpgid システムコールも使用できるがフォアグラウンドかどうかも検査するためにこちらを使う
            shell_pgid: tcgetpgrp(libc::STDIN_FILENO).unwrap(),
        }
    }


    // worker_rx: worker の receiver
    // shell_tx: shell の SyncSender
    // fn spawn(mut self, worker_rx: Receiver<WorkerMsg>, shell_tx: SyncSender<ShellMsg>) {
    //     thread::spawn(move || {
    //        for msg in worker_rx.iter() { // worker_rx から受信
    //            match msg {
    //                WorkerMsg::Cmd(line) => {
    //                    match parse_cmd(&line) { // メッセージパース
    //                        Ok(cmd) => {
    //                            // ★組み込みコマンド 実行
    //                            if self.built_in_cmd(&cmd, &shell_tx) {
    //                                // 完了したら、worker_rx から受信を再開
    //                                continue;
    //                            }
    //
    //                            // ★組み込みコマンド以外は子プロセス生成して、外部プログラム実行
    //                            if !self.spawn_child(&line, &cmd) {
    //                                // 子プロセス生成に失敗した場合
    //                                // シェルからの入力を再開、mainスレッドに通知
    //                                shell_tx.send(
    //                                    ShellMsg::Continue(self.exit_val)
    //                                ).unwrap();
    //                            }
    //                        }
    //                        Err(e) => {
    //                            eprintln!("ZeroSh: {e}");
    //                            shell_tx.send(
    //                                ShellMsg::Continue(self.exit_val)
    //                            ).unwrap();
    //                        }
    //                    }
    //                }
    //                WorkerMsg::Signal(SIGCHLD) => {
    //                    self.wait_child(&shell_tx); // ★子プロセスの状態変化管理
    //                }
    //                _ => (), // 無視
    //            }
    //        }
    //     });
    // }

    // 組み込みコマンドの実行、実行したら true を返す
    fn build_in_cmd(&mut self,
                    cmd: &[(&str, Vec<&str>)],
                    shell_tx: &SyncSender<ShellMsg>) -> bool {
        if cmd.len() > 1 {
            // 組み込みコマンドのパイプはサポートしない
            return false;
        }

        match cmd[0].0 {
            "exit" => self.run_exit(&cmd[0].1, shell_tx),
            // "jobs" => self.run_jobs(shell_tx), // ジョブ一覧表示
            "fg" => self.run_fg(&cmd[0].1, shell_tx),
            // "cd" => self.run_cd(&cmd[0].1, shall_tx),
            _ => false,
        }
    }

    fn run_exit(&mut self, args: &[&str], shell_tx: &SyncSender<ShellMsg>) -> bool {
        // 実行中ジョブがある場合はエラー
        if !self.jobs.is_empty() {
            eprintln!("ZeroSh: ジョブが実行中です");
            self.exit_val = 1;
            shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap(); // shell に再開を通知
            return true;
        }

        // 終了コード取得
        let exit_val = if let Some(s) = args.get(1) {
            if let Ok(n) = (*s).parse::<i32>() {
                n
            } else {
                eprintln!("{s} は不正な引数です");
                self.exit_val = 1;
                shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap(); // shell に再開を通知
                return true;
            }
        } else {
            self.exit_val
        };

        shell_tx.send(ShellMsg::Quit(exit_val)).unwrap(); // shell に終了を通知
        true
    }

    fn run_fg(&mut self, args:  &[&str], shell_tx: &SyncSender<ShellMsg>) -> bool {
        // TODO: self 変更の必要ある？
        self.exit_val = 1;

        // 引数チェック
        if args.len() < 2 {
            eprintln!("ZeroSh: fg: ジョブIDまたはプロセスIDを指定してください");
            shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap();
            return true;
        }

        // ジョブID取得
        if let Ok(n) = args[1].parse::<usize>() {
            if let Some((pgid, cmd)) = self.jobs.get(&n) {
                eprintln!("[{n}] 再開\t{cmd}");

                self.fg = Some(*pgid);
                // ジョブをフォアグラウンドに移動
                // ファイルディスクリプタ + プロセスグループID
                // ファイルディスクリプタに関連付けられたセッションのフォアグラウンドプロセスグループを、指定されたプロセスグループにする
                tcsetpgrp(libc::STDIN_FILENO, *pgid).unwrap();

                // ジョブの実行を再開
                //   そもそもフォアグラウンドプロセスを変更した場合は、シェアルの読み込みは再開しない
                killpg(*pgid, Signal::SIGCONT).unwrap();
                return true;
            }
        }

        // 失敗
        eprintln!("{} というジョブは見つかりませんでした", args[1]);
        shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap(); // shell に再開を通知
        true
    }

    fn spawn_child(&mut self, line: &str, cmd: &[(&str, Vec<&str>)]) -> bool {
        assert_ne!(cmd.len(), 0); // コマンドが空の場合はありえない

        // job id 取得
        let job_id = if let Some(id) = self.get_new_job_id() {
            id
        } else {
            eprintln!("ZeroSh: ジョブ数が上限に達しました");
            return false;
        };

        if cmd.len() > 2 {
            eprintln!("ZeroSh: 3つ以上のコマンドパイプはサポートしていません");
            return false
        }

        let mut input = None; // 2つ目のプロセスの標準入力
        let mut output = None; // 1つ目のプロセスの標準出力
        if cmd.len() == 2 {
            // パイプの作成
            let p = pipe().unwrap();
            input = Some(p.0);
            output = Some(p.1);
        }

        // ★パイプは自前でcloseする必要がある
        let cleanup_pipe = CleanUp {
            f: || {
                if let Some(fd) = input {
                    syscall(|| unistd::close(fd)).unwrap();
                }
                if let Some(fd) = output {
                    syscall(|| unistd::close(fd)).unwrap();
                }
            }
        };

        let pgid;
        // 1つ目のプロセスを生成
        match fork_exec(Pid::from_raw(0), cmd[0].0, &cmd[0].1, None, output) {
            Ok(child) => {
                pgid = child;
            }
            Err(e) => {
                eprintln!("Zerosh: プロセスの生成に失敗しました: {e}");
                return false;
            }
        }

        // プロセス、ジョブの情報追加
        let info = ProcInfo {
            state: ProcState::Run,
            pgid,
        };
        let mut pids = HashMap::new();
        pids.insert(pgid, info.close());

        // 2つ目のプロセスを生成
        if cmd.len() == 2 {
            match fork_exec(pgid, cmd[1].0, &cmd[1].1, input, None) {
                Ok(child) => {
                    pids.insert(child, info);
                }
                Err(e) => {
                    eprintln!("Zerosh: プロセスの生成に失敗しました: {e}");
                    return false
                }
            }
        }

        std::mem::drop(cleanup_pipe); // パイプを閉じる

        // ジョブ情報追加して、子プロセスをフォアグラウンドプロセスグループにする
        self.fg = Some(pgid);
        self.insert_job(job_id, pgid, pids, line);

        tcsetpgrp(libc::STDIN_FILENO, pgid).unwrap();

        true
    }
}

fn fork_exec(pgid: Pid, filename: &str, args: &[&str], input: Option<i32>, output: Option<i32>) -> Result<Pid, DynError> {
    // ??
    let filename = CString::new(filename).unwrap();
    // CString に変換
    let args: Vec<CString> = args.iter().map(|s| CString::new(*s).unwrap()).collect();

    // 子プロセスを生成
    // 戻り値が親と子で2回返る
    match syscall(|| unsafe { fork() })? {
        ForkResult::Parent { child, .. } => {
            // 子プロセスのプロセスグループIDを pgid に設定
            // 親子両方実施するのは、どちらが先に実行されるか決定不能なため
            setpgid(child, pgid).unwrap();
            Ok(child)
        }
        ForkResult::Child => {
            // 子プロセスのプロセスグループIDを pgid に設定
            setpgid(Pid::from_raw(0), pgid).unwrap();

            // 標準入出力を設定
            if let Some(infd) = input {
                syscall(|| dup2(infd, libc::STDIN_FILENO)).unwrap();
            }
            if let Some(outfd) = output {
                syscall(|| dup2(outfd, libc::STDOUT_FILENO)).unwrap();
            }

            // signal_hook で利用される Unix ドメインソケットと pipe を閉じる
            for i in 3..= 6 {
                let _ = syscall(|| unistd::close(i));
            }

            match execvp(&filename, &args) {
                Err(_) => {
                    unistd::write(libc::STDERR_FILENO, "不明なコマンド実行\n".as_bytes()).ok();
                    exit(1);
                }
                Ok(_) => unreachable!(),

            }
        }
    }
}

type CmdResult<'a> = Result<Vec<(&'a str, Vec<&'a str>)>, DynError>;

// "echo hello | less" -> vec![("echo", vec!["hello"]), ("less", vec![])]
// ベクタの要素は パイプ で区切られた処理
// 第0要素がコマンド、第1要素が引数
fn parse_cmd(line: &str) -> CmdResult {
    // '|' で split
    // 各要素を ' ' で split
    //   ただし、空白文字列は無視するか、パイプの先にコマンドが指定されていない場合はエラー
    let cmds = parse_pipe(line);
    if cmds.is_empty() {
        return Err("空のコマンド".into());
    }

    let mut result = Vec::new();
    for cmd in cmds {
        let (filename, args) = parse_cmd_one(cmd)?;
        result.push((filename, args));
    }

    Ok(result)
}

/// パイプでsplit
fn parse_pipe(line: &str) -> Vec<&str> {
    let cmds: Vec<&str> = line.split('|').collect();
    cmds
}

/// スペースでsplit
fn parse_cmd_one(line: &str) -> Result<(&str, Vec<&str>), DynError> {
    let cmd: Vec<&str> = line.split_whitespace().collect();
    let mut filename = "";
    let mut args = Vec::new(); // 引数を生成。ただし、空の文字列filterで取り除く
    for (n, s) in cmd.iter().filter(|s| !s.is_empty()).enumerate() {
        if n == 0 {
            filename = *s;
            continue; // これなかった？
        }
        args.push(*s);
    }

    if filename.is_empty() {
        Err("空のコマンド".into())
    } else {
        Ok((filename, args))
    }
}

struct CleanUp<F> where F: Fn(),
{
    f: F,
}

impl<F> Drop for CleanUp<F> where F: Fn() {
    fn drop(&mut self) {
        (self.f)();
    }
}

#[cfg(test)]
mod tests {
    use crate::shell::parse_cmd;

    #[test]
    fn test_parse_cmd() {
        let line = "echo hello | less";
        let expected = vec![("echo", vec!["hello"]), ("less", vec![])];
        assert_eq!(parse_cmd(line).unwrap(), expected);
        // println!("{:?}", parse_cmd(line).unwrap());
    }
}
