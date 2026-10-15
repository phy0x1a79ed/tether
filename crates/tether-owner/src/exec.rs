//! Running what the operator asks for, on the owner's machine.
//!
//! Two shapes of task, because two shapes of work exist and one of them is
//! invisible otherwise. A [`Kind::Command`] runs to completion and its output is
//! a block of text in the owner's transcript. A [`Kind::Shell`] is a real
//! pseudo-terminal, which is the only way a full-screen program — an editor, a
//! progress display, anything that draws — reaches the owner as the screen the
//! operator is actually looking at rather than as a spray of escape codes.
//!
//! # The ordering rule this module has to honour
//!
//! Every `Output` for a task reaches the wire before that task's `Exit`, and
//! `Exit` is the last thing a task produces. A transcript that showed an exit
//! status above the output it belongs to would be lying about what happened on
//! the owner's machine, which is the one thing this tool cannot do.
//!
//! It holds because a task's frames are written into one channel, and because
//! the `Exit` is sent only after both readers have run dry. A second path for
//! output — a thread with its own sender, a fast path for large chunks — would
//! break it without failing anything that does not watch the order.
//!
//! # Nothing here runs before consent
//!
//! A [`Runner`] is constructed after the person at the keyboard has said yes,
//! and the session loop refuses every task-shaped frame that arrives before
//! then. Two independent guards for one rule, because it is the rule.

use std::collections::HashMap;

use portable_pty::{CommandBuilder, MasterPty, PtySize};

use crate::platform::{command_flag, default_shell, home_dir};
use tether_proto::frame::{Ended, Frame, Kind, Stream, TaskId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// How much of a stream is carried in one frame.
///
/// Well under the frame limit so a chunk plus its encoding always fits, and
/// large enough that a chatty build is not thousands of frames a second.
const CHUNK: usize = 32 * 1024;

/// What a running task will accept from this side.
enum Control {
    /// Bytes for the task's input. Empty means there is no more, which is the
    /// only way a command that reads to end-of-file ever finishes.
    Input(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Stop it. Distinct from end-of-input on purpose: stopping a program that
    /// is still working is a different instruction from telling it the input
    /// has run out.
    Close,
}

pub struct Runner {
    tasks: HashMap<TaskId, mpsc::Sender<Control>>,
    out: mpsc::Sender<Frame>,
    shell: String,
}

impl Runner {
    /// Build the runner. Called only once consent has been given.
    pub fn new(out: mpsc::Sender<Frame>) -> Self {
        Self {
            tasks: HashMap::new(),
            out,
            shell: default_shell(),
        }
    }

    pub fn open(
        &mut self,
        task: TaskId,
        kind: Kind,
        command: Option<String>,
        cols: u16,
        rows: u16,
    ) {
        if self.tasks.contains_key(&task) {
            // A repeated task id is the operator's bug, and honouring it would
            // interleave two programs' output under one heading.
            return;
        }
        let (tx, rx) = mpsc::channel(64);
        self.tasks.insert(task, tx);
        let out = self.out.clone();
        let shell = self.shell.clone();
        match kind {
            Kind::Command => {
                let line = command.unwrap_or_else(|| "true".into());
                tokio::spawn(run_command(task, shell, line, out, rx));
            }
            Kind::Shell => {
                tokio::spawn(run_shell(task, shell, command, cols, rows, out, rx));
            }
        }
    }

    pub async fn input(&mut self, task: TaskId, data: Vec<u8>) {
        self.send(task, Control::Input(data)).await;
    }

    pub async fn resize(&mut self, task: TaskId, cols: u16, rows: u16) {
        self.send(task, Control::Resize { cols, rows }).await;
    }

    pub async fn close(&mut self, task: TaskId) {
        self.send(task, Control::Close).await;
    }

    /// Stop everything. The owner cut the session, or the operator did.
    pub async fn shutdown(&mut self) {
        for (_, tx) in self.tasks.drain() {
            let _ = tx.send(Control::Close).await;
        }
    }

    pub fn forget(&mut self, task: TaskId) {
        self.tasks.remove(&task);
    }

    async fn send(&mut self, task: TaskId, control: Control) {
        if let Some(tx) = self.tasks.get(&task) {
            if tx.send(control).await.is_err() {
                self.tasks.remove(&task);
            }
        }
    }
}

/// A command, run through the owner's shell so that pipes and redirections mean
/// what the operator typed.
async fn run_command(
    task: TaskId,
    shell: String,
    line: String,
    out: mpsc::Sender<Frame>,
    mut control: mpsc::Receiver<Control>,
) {
    let mut child = match tokio::process::Command::new(&shell)
        .arg(command_flag(&shell))
        .arg(&line)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            let _ = out
                .send(Frame::Output {
                    task,
                    stream: Stream::Stderr,
                    data: format!("tether could not start {shell}: {e}\n").into_bytes(),
                })
                .await;
            let _ = out
                .send(Frame::Exit {
                    task,
                    ended: Ended::Code(127),
                })
                .await;
            return;
        }
    };

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    // Held in an Option because dropping it is how end-of-input is delivered.
    let mut stdin = Some(child.stdin.take().expect("stdin was piped"));

    let readers = tokio::spawn({
        let out = out.clone();
        async move {
            // Both streams are drained before the exit is announced, which is
            // what keeps a task's output above its own exit status.
            let a = tokio::spawn(pump_stream(task, Stream::Stdout, stdout, out.clone()));
            let b = tokio::spawn(pump_stream(task, Stream::Stderr, stderr, out));
            let _ = tokio::join!(a, b);
        }
    });

    let (kill, mut killed) = tokio::sync::oneshot::channel::<()>();
    let feeder = tokio::spawn(async move {
        let mut kill = Some(kill);
        while let Some(control) = control.recv().await {
            match control {
                // Dropping the handle is the end-of-file the far end asked for.
                Control::Input(data) if data.is_empty() => stdin = None,
                Control::Input(data) => {
                    let Some(handle) = stdin.as_mut() else {
                        continue;
                    };
                    if handle.write_all(&data).await.is_err() {
                        stdin = None;
                        continue;
                    }
                    let _ = handle.flush().await;
                }
                // A command has no terminal to resize.
                Control::Resize { .. } => {}
                Control::Close => {
                    if let Some(kill) = kill.take() {
                        let _ = kill.send(());
                    }
                    break;
                }
            }
        }
        // The session ending drops the sender, which lands here: a command
        // still reading its input must see the end of it, or it never exits.
        drop(stdin);
    });

    let status = tokio::select! {
        status = child.wait() => status,
        _ = &mut killed => {
            let _ = child.start_kill();
            child.wait().await
        }
    };
    let _ = readers.await;
    feeder.abort();

    let ended = match status {
        Ok(status) => match status.code() {
            Some(code) => Ended::Code(code),
            None => Ended::Signal(signal_of(&status)),
        },
        Err(_) => Ended::Code(-1),
    };
    let _ = out.send(Frame::Exit { task, ended }).await;
}

#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.signal().unwrap_or(0)
}

#[cfg(not(unix))]
fn signal_of(_status: &std::process::ExitStatus) -> i32 {
    0
}

async fn pump_stream<R>(task: TaskId, stream: Stream, mut source: R, out: mpsc::Sender<Frame>)
where
    R: AsyncReadExt + Unpin,
{
    let mut buf = vec![0u8; CHUNK];
    loop {
        match source.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out
                    .send(Frame::Output {
                        task,
                        stream,
                        data: buf[..n].to_vec(),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}

/// A pseudo-terminal, which is what makes an interactive program visible.
///
/// The pty crate is blocking by nature — a terminal is a file descriptor with
/// no async story — so its reader and writer live on blocking threads and talk
/// to the session over channels. That is the boundary, and it is why there is
/// no attempt to poll a pty from the runtime.
#[allow(clippy::too_many_arguments)]
async fn run_shell(
    task: TaskId,
    shell: String,
    command: Option<String>,
    cols: u16,
    rows: u16,
    out: mpsc::Sender<Frame>,
    mut control: mpsc::Receiver<Control>,
) {
    let size = PtySize {
        rows: rows.max(1),
        cols: cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    };
    let pty = match portable_pty::native_pty_system().openpty(size) {
        Ok(pty) => pty,
        Err(e) => {
            let _ = out
                .send(Frame::Output {
                    task,
                    stream: Stream::Screen,
                    data: format!("tether could not open a terminal: {e}\r\n").into_bytes(),
                })
                .await;
            let _ = out
                .send(Frame::Exit {
                    task,
                    ended: Ended::Code(127),
                })
                .await;
            return;
        }
    };

    let mut builder = CommandBuilder::new(command.unwrap_or(shell));
    // A shell with no TERM draws nothing useful, and the owner would be
    // watching a blank rectangle wondering what went wrong.
    builder.env(
        "TERM",
        std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
    );
    if let Some(home) = home_dir() {
        builder.cwd(home);
    }

    let mut child = match pty.slave.spawn_command(builder) {
        Ok(child) => child,
        Err(e) => {
            let _ = out
                .send(Frame::Output {
                    task,
                    stream: Stream::Screen,
                    data: format!("tether could not start a shell: {e}\r\n").into_bytes(),
                })
                .await;
            let _ = out
                .send(Frame::Exit {
                    task,
                    ended: Ended::Code(127),
                })
                .await;
            return;
        }
    };
    // The slave end must be closed here or the shell never sees end-of-file.
    drop(pty.slave);

    let mut reader = pty.master.try_clone_reader().expect("the pty can be read");
    let mut writer = pty.master.take_writer().expect("the pty can be written");
    let master: Box<dyn MasterPty + Send> = pty.master;

    let screen = tokio::task::spawn_blocking({
        let out = out.clone();
        move || {
            use std::io::Read;
            let mut buf = vec![0u8; CHUNK];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if out
                            .blocking_send(Frame::Output {
                                task,
                                stream: Stream::Screen,
                                data: buf[..n].to_vec(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        }
    });

    let killer = child.clone_killer();
    let driver = tokio::spawn(async move {
        while let Some(control) = control.recv().await {
            match control {
                // A terminal has no end-of-file to deliver; the operator
                // sends an actual ^D byte when that is what they mean.
                Control::Input(data) if data.is_empty() => {}
                Control::Input(data) => {
                    let ok = tokio::task::block_in_place(|| {
                        use std::io::Write;
                        writer.write_all(&data).and_then(|_| writer.flush()).is_ok()
                    });
                    if !ok {
                        break;
                    }
                }
                Control::Resize { cols, rows } => {
                    let _ = master.resize(PtySize {
                        rows: rows.max(1),
                        cols: cols.max(1),
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                Control::Close => break,
            }
        }
        let mut killer = killer;
        let _ = killer.kill();
    });

    let status = tokio::task::spawn_blocking(move || child.wait()).await;
    // The driver is stopped first, which drops the master end of the pty. On
    // Unix that changes nothing — the shell has exited, the slave was dropped
    // at spawn, and the reader has already seen end-of-file. On Windows it is
    // what *delivers* end-of-file: a console pty holds its output pipe open
    // until the pty itself is closed, so a reader joined before this waits for
    // a program that is already gone.
    driver.abort();
    // The screen reader is joined before the exit goes out, for the same reason
    // a command's readers are: the last thing drawn must arrive before the note
    // saying the program is gone.
    let _ = screen.await;

    let ended = match status {
        Ok(Ok(status)) => Ended::Code(status.exit_code() as i32),
        _ => Ended::Code(-1),
    };
    let _ = out.send(Frame::Exit { task, ended }).await;
}
