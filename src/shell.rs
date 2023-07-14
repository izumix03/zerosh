use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::exit;
use std::sync::mpsc::{channel, Sender, sync_channel};
use std::thread;
use nix::libc;

use nix::libc::{SIGINT, SIGTSTP, SIGCHLD, tcgetpgrp};
use nix::sys::signal::{SigHandler, Signal, signal};
use nix::unistd::Pid;
use rustyline::{DefaultEditor};
use rustyline::error::ReadlineError;
use signal_hook::iterator::Signals;

use crate::helper::DynError;

/// システムコール呼び出しのラッパ
/// EINTR(システム割り込み) ならリトライ
///
/// f:  システムコールを呼び出す関数
fn syscall<F, T>(f: F) -> Result<T, nix::Error>
    where
        F: Fn() -> Result<T, nix::Error> ,
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
    Signal(i32), // シグナルを受信
    Cmd(String), // コマンド入力
}

/// main スレッドが受信するメッセージ
enum ShellMsg {
    Continue(i32), // シェルの
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
    pub fn run (&self) -> Result<(), DynError> {
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
            let face = if prev == 0 { '\u{1F642}' } else { '\u{1F480}'};
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
                        },
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

    pid_to_info: HashMap<Pid, ProcInfo>, // プロセスIDから プロセスグループIDへのマップ
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
            shell_pgid: tcgetpgrp(libc::STDIN_FILENO).unwrap(),
        }
    }
}