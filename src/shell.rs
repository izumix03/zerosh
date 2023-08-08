use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::CString;
use std::mem::replace;
use std::path::PathBuf;
use std::process::exit;
use std::sync::mpsc::{channel, Receiver, Sender, sync_channel, SyncSender};
use std::thread;

use nix::{libc, unistd};
use nix::libc::{SIGCHLD, SIGINT, SIGTSTP};
use nix::sys::signal::{killpg, SigHandler, Signal, signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
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
        Worker::new().spawn(worker_rx, shell_tx);

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
    pgid_to_pids: HashMap<Pid, (usize, HashSet<Pid>)>,

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
            pgid_to_pids: HashMap::new(),
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
    fn spawn(mut self, worker_rx: Receiver<WorkerMsg>, shell_tx: SyncSender<ShellMsg>) {
        thread::spawn(move || {
           for msg in worker_rx.iter() { // worker_rx から受信
               match msg {
                   WorkerMsg::Cmd(line) => {
                       match parse_cmd(&line) { // メッセージパース
                           Ok(cmd) => {
                               // ★組み込みコマンド 実行
                               if self.built_in_cmd(&cmd, &shell_tx) {
                                   // 完了したら、worker_rx から受信を再開
                                   continue;
                               }

                               // ★組み込みコマンド以外は子プロセス生成して、外部プログラム実行
                               if !self.spawn_child(&line, &cmd) {
                                   // 子プロセス生成に失敗した場合
                                   // シェルからの入力を再開、mainスレッドに通知
                                   shell_tx.send(
                                       ShellMsg::Continue(self.exit_val)
                                   ).unwrap();
                               }
                           }
                           Err(e) => {
                               eprintln!("ZeroSh: {e}");
                               shell_tx.send(
                                   ShellMsg::Continue(self.exit_val)
                               ).unwrap();
                           }
                       }
                   }
                   WorkerMsg::Signal(SIGCHLD) => {
                       self.wait_child(&shell_tx); // ★子プロセスの状態変化管理
                   }
                   _ => (), // 無視
               }
           }
        });
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
        pids.insert(pgid, info.clone());

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

    /// 組み込みコマンドの場合はtrueを返す
    fn built_in_cmd(&mut self, cmd: &[(&str, Vec<&str>)], shell_tx: &SyncSender<ShellMsg>) -> bool {
        if cmd.len() > 1 {
            return false; // 組み込みコマンドのパイプは非対応なのでエラー
        }

        match cmd[0].0 {
            "exit" => self.run_exit(&cmd[0].1, shell_tx),
            "jobs" => self.run_jobs(shell_tx),
            "fg" => self.run_fg(&cmd[0].1, shell_tx),
            "cd" => self.run_cd(&cmd[0].1, shell_tx),
            _ => false,
        }
    }

    /// jobsコマンドを実行
    fn run_jobs(&mut self, shell_tx: &SyncSender<ShellMsg>) -> bool {
        for (job_id, (pgid, cmd)) in &self.jobs {
            let state = if self.is_group_stop(*pgid).unwrap() {
                "停止中"
            } else {
                "実行中"
            };
            println!("[{job_id}] {state}\t{cmd}")
        }
        self.exit_val = 0; // 成功
        shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap(); // シェルを再開
        true
    }

    /// カレントディレクトリを変更。引数がない場合は、ホームディレクトリに移動。第2引数以降は無視
    fn run_cd(&mut self, args: &[&str], shell_tx: &SyncSender<ShellMsg>) -> bool {
        let path = if args.len() == 1 {
            // 引数が指定されていない場合、ホームディレクトリか/へ移動
            dirs::home_dir()
                .or_else(|| Some(PathBuf::from("/")))
                .unwrap()
        } else {
            PathBuf::from(args[1])
        };

        // カレントディレクトリを変更
        if let Err(e) = std::env::set_current_dir(&path) {
            self.exit_val = 1; // 失敗
            eprintln!("cdに失敗: {e}");
        } else {
            self.exit_val = 0; // 成功
        }

        shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap();
        true
    }


    // 子プロセスの状態管理
    fn wait_child(&mut self, shell_tx: &SyncSender<ShellMsg>) {
        // WUNTRACED: 停止
        // WNOHANG: ブロックしない
        // WCONTINUED: 実行再開
        // waitpid: 子プロセスの終了を待つ
        let flag = Some(WaitPidFlag::WUNTRACED | WaitPidFlag::WNOHANG | WaitPidFlag::WCONTINUED);
        loop {
            match syscall(|| waitpid(Pid::from_raw(-1), flag)) {
                Ok(WaitStatus::Exited(pid, status)) => {
                    // プロセス終了
                    self.exit_val = status; // 終了ステータスを保存
                    // ??
                    self.process_term(pid, shell_tx);
                }
                Ok(WaitStatus::Signaled(pid, sig, core)) => {
                    // シグナルによる終了
                    eprintln!("プロセス {pid} がシグナル {sig} で終了しました{}", if core {
                        "(コアダンプ)" // ??
                    } else {
                        ""
                    });
                    // ?? なんで 128 たすの？
                    self.exit_val = sig as i32 + 128; // 終了ステータスを保存
                    self.process_term(pid, shell_tx);
                }
                Ok(WaitStatus::Stopped(pid, _sig)) => {
                    // プロセスが停止
                    self.process_stop(pid, shell_tx);
                }
                Ok(WaitStatus::Continued(pid)) => {
                    // プロセスが再開
                    self.process_continue(pid);
                }
                Ok(WaitStatus::StillAlive) => {
                    // まだプロセスが生きている
                    return;
                }
                Err(nix::Error::ECHILD) => {
                    // 子プロセスがいない
                    return;
                }
                Err(e) => {
                    eprintln!("failed to wait: {}", e);
                    exit(1);
                }

                #[cfg(any(target_os = "linux", target_os = "android"))]
                Ok(WaitStatus::PtraceEvent(pid, sig, _) | WaitStatus::PtraceSyscall(pid)) => {
                    // プロセスが停止
                    self.process_stop(pid, shell_tx);
                }
            }
        }
    }

    // プロセス終了
    fn process_term(&mut self, pid: Pid, shell_tx: &SyncSender<ShellMsg>) {
        // プロセスIDを削除し、必要ならフォアグラウンドプロセスをシェルに設定
        if let Some((job_id, pgid)) = self.remove_pid(pid) {
            self.manage_job(job_id, pgid, shell_tx);
        }
    }

    // プロセス停止
    fn process_stop(&mut self, pid: Pid, shell_tx: &SyncSender<ShellMsg>) {
        // プロセスを停止中に設定。？
        self.set_pid_state(pid, ProcState::Stop);
        // プロセスグループID取得
        let pgid = self.pid_to_info.get(&pid).unwrap().pgid;
        let job_id = self.pgid_to_pids.get(&pgid).unwrap().0;
        // 必要ならフォアグラウンドプロセスをシェルに設定
        self.manage_job(job_id, pgid, shell_tx);
    }

    // プロセス再開
    fn process_continue(&mut self, pid: Pid ){
        self.set_pid_state(pid, ProcState::Run);
    }

    fn manage_job(&mut self, job_id: usize, pgid: Pid, shell_tx: &SyncSender<ShellMsg>) {
        let is_fg = self.fg.map_or(false, |fg| fg == pgid);
        let line = &self.jobs.get(&job_id).unwrap().1;
        if is_fg {
            // 状態が変化したプロセスはフォアグラウンドに設定する
            if self.is_group_empty(pgid) {
                // フォアグラウンドプロセスが空の場合、
                // ジョブ情報を削除してシェルをフォアグラウンドに設定
                eprintln!("[{job_id}]終了\t{line}");
                self.remove_job(job_id);
                // シェルをフォアグラウンドに設定、シェルの入力を再開
                self.set_shell_fg(shell_tx);
            } else if self.is_group_stop(pgid).unwrap() {
                // フォアグラウンドプロセスが全て停止中の場合、シェルをフォアグラウンドに設定
                eprintln!("[{job_id}]停止\t{line}");
                // シェルをフォアグラウンドに設定、シェルの入力を再開
                self.set_shell_fg(shell_tx);
            }
        } else {
            if self.is_group_empty(pgid) {
                eprintln!("\n[{job_id}]終了\t{line}");
                // プロセスグループが空の場合、ジョブ情報を削除
                self.remove_job(job_id);
            }
        }
    }

    fn insert_job(&mut self, job_id: usize, pgid: Pid, pids: HashMap<Pid, ProcInfo>, line: &str) {
        assert!(!self.jobs.contains_key(&job_id));
        // ジョブ情報を追加
        self.jobs.insert(job_id, (pgid, line.to_string()));

        // pgid_to_pids にプロセスグループIDとプロセスIDの対応を追加
        // value 側
        let mut procs = HashSet::new();
        for (pid, info) in pids {
            procs.insert(pid);

            assert!(!self.pid_to_info.contains_key(&pid));
            // プロセス情報を追加
            self.pid_to_info.insert(pid, info);
        }

        assert!(!self.pgid_to_pids.contains_key(&pgid));
        self.pgid_to_pids.insert(pgid, (job_id, procs));
    }

    // プロセスの状態を設定し、以前の状態を返す
    // pid が存在しない場合は None を返す
    fn set_pid_state(&mut self, pid: Pid, state: ProcState) -> Option<ProcState> {
        let info = self.pid_to_info.get_mut(&pid)?;
        Some(replace(&mut info.state, state))
    }

    // プロセス情報を削除して、削除できた場合はプロセスの所属する(ジョブID,プロセスグループID)を返す
    // 存在しない場合はNoneを返す
    fn remove_pid(&mut self, pid: Pid) -> Option<(usize, Pid)> {
        // プロセスグループID取得
        let pgid = self.pid_to_info.get(&pid)?.pgid;
        let it = self.pgid_to_pids.get_mut(&pgid)?;
        // プロセスグループからpid削除
        it.1.remove(&pid);
        // ジョブIDを取得
        let job_id = it.0;
        Some((job_id, pgid))
    }

    fn remove_job(&mut self, job_id: usize) {
        if let Some((pgid, _)) = self.jobs.remove(&job_id) {
            if let Some((_, pids)) = self.pgid_to_pids.remove(&pgid) {
                assert!(pids.is_empty()); // ジョブ削除時はプロセスグループは空のはず
            }
        }
    }

    // 空のプロセスグループならtrueを返す
    fn is_group_empty(&self, pgid: Pid) -> bool {
        self.pgid_to_pids.get(&pgid).unwrap().1.is_empty()
    }

    // プロセスグループのプロセスすべてが停止中ならtrueを返す
    fn is_group_stop(&self, pgid: Pid) -> Option<bool> {
        for pid in self.pgid_to_pids.get(&pgid)?.1.iter() {
            if self.pid_to_info.get(pid).unwrap().state == ProcState::Run {
                return Some(false);
            }
        }
        Some(true)
    }

    // シェルをフォアグラウンドに設定、シェルの入力を再開
    fn set_shell_fg(&mut self, shell_tx: &SyncSender<ShellMsg>) {
        self.fg = None;
        tcsetpgrp(libc::STDIN_FILENO, self.shell_pgid).unwrap();
        shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap();
    }

    // 新たなジョブIDを取得
    fn get_new_job_id(&self) -> Option<usize> {
        for i in 0..=usize::MAX {
            if !self.jobs.contains_key(&i) {
                return Some(i);
            }
        }
        None
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
