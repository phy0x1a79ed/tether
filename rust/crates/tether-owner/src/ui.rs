//! What the owner watches while it happens.
//!
//! The display never sees a [`Frame`](tether_proto::frame::Frame). It consumes
//! [`Event`], which the session loop translates into, and that seam is
//! deliberate: it is what let the predecessor's interface survive a change of
//! transport, and it is what will let this one survive the next.
//!
//! # Two views, because there are two kinds of work
//!
//! Line-oriented work is a transcript: a command, its output, its exit status,
//! in that order. An interactive program cannot be a transcript at all — it
//! draws, it moves the cursor, it redraws in place — so the bytes go through a
//! terminal parser and the owner sees the **screen**, the same one the operator
//! is looking at. Showing escape codes instead would technically be "visible"
//! and would tell the owner nothing.
//!
//! # Why the transcript is sanitised and the screen is not
//!
//! A command's output is written into the owner's own terminal, so an escape
//! sequence in it could move the cursor, clear the display, or overwrite the
//! header. On a tool whose entire premise is that the owner sees what is
//! happening, output that can hide itself is not acceptable, so control bytes
//! are shown rather than executed. The live screen is different in kind: those
//! bytes are interpreted by a parser into a grid of characters and only the
//! grid is drawn, so nothing from the far end is ever executed by the owner's
//! terminal.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::time::Instant;

use crossterm::{cursor, event, execute, queue, style, terminal};
use tether_proto::frame::{Ended, Hello, Kind, Stream, TaskId};

/// How much scrollback the transcript keeps.
const SCROLLBACK: usize = 2000;

/// What the display is told. Nothing here names the wire.
#[derive(Debug, Clone)]
pub enum Event {
    Said {
        from_operator: bool,
        text: String,
    },
    /// The size is the **operator's** terminal, not the owner's. The parser
    /// models the screen the operator is looking at, and the owner's view
    /// shows as much of that grid as fits. Modelling the owner's size instead
    /// would mean the two people are watching different screens.
    Started {
        task: TaskId,
        kind: Kind,
        command: Option<String>,
        cols: u16,
        rows: u16,
    },
    Resized {
        task: TaskId,
        cols: u16,
        rows: u16,
    },
    Output {
        task: TaskId,
        stream: Stream,
        data: Vec<u8>,
    },
    Ended {
        task: TaskId,
        ended: Ended,
    },
    /// Something the client itself wants to say, rather than either person.
    Note(String),
    Cut {
        reason: String,
    },
    /// Redraw, so the elapsed time keeps moving.
    Tick,
}

/// What is in the header for as long as the session lasts.
pub struct Facts {
    pub operator: String,
    pub relay: String,
    pub code: String,
}

impl Facts {
    pub fn new(operator: &Hello, relay: String, code: String) -> Self {
        Self {
            operator: format!("{} on {}", operator.who, operator.host),
            relay,
            code,
        }
    }
}

struct Live {
    task: TaskId,
    parser: vt100::Parser,
}

pub struct Ui {
    facts: Facts,
    started: Instant,
    transcript: VecDeque<String>,
    pending: String,
    live: Option<Live>,
    /// False when the owner is not on a terminal — a log, a pipe, a test. The
    /// session still runs and is still visible; it is simply printed.
    tty: bool,
    cols: u16,
    rows: u16,
}

impl Ui {
    pub fn new(facts: Facts) -> io::Result<Self> {
        let tty = std::io::IsTerminal::is_terminal(&io::stdout());
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let mut ui = Self {
            facts,
            started: Instant::now(),
            transcript: VecDeque::new(),
            pending: String::new(),
            live: None,
            tty,
            cols,
            rows,
        };
        if tty {
            terminal::enable_raw_mode()?;
            execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
        }
        ui.line(format!(
            "session open with {} through {}",
            ui.facts.operator, ui.facts.relay
        ));
        Ok(ui)
    }

    pub fn tty(&self) -> bool {
        self.tty
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Said {
                from_operator,
                text,
            } => {
                let who = if from_operator { "them" } else { "you" };
                self.line(format!("{who}: {text}"));
            }
            Event::Started {
                task,
                kind,
                command,
                cols,
                rows,
            } => {
                let what = command.unwrap_or_else(|| "login shell".into());
                match kind {
                    Kind::Command => self.line(format!("$ {what}")),
                    Kind::Shell => {
                        self.line(format!("[{task}] terminal: {what}"));
                        self.live = Some(Live {
                            task,
                            parser: vt100::Parser::new(rows.max(1), cols.max(1), SCROLLBACK),
                        });
                    }
                }
            }
            Event::Resized { task, cols, rows } => {
                if let Some(live) = self.live.as_mut() {
                    if live.task == task {
                        live.parser.screen_mut().set_size(rows.max(1), cols.max(1));
                    }
                }
            }
            Event::Output { task, stream, data } => match stream {
                Stream::Screen => {
                    if let Some(live) = self.live.as_mut() {
                        if live.task == task {
                            live.parser.process(&data);
                        }
                    }
                }
                _ => self.write_transcript(&data),
            },
            Event::Ended { task, ended } => {
                self.flush_pending();
                if self.live.as_ref().is_some_and(|l| l.task == task) {
                    self.live = None;
                }
                self.line(match ended {
                    Ended::Code(0) => "  done".to_string(),
                    Ended::Code(code) => format!("  exit {code}"),
                    Ended::Signal(sig) => format!("  killed by signal {sig}"),
                });
            }
            Event::Note(text) => self.line(format!("· {text}")),
            Event::Cut { reason } => {
                self.live = None;
                self.line(format!("session cut: {reason}"));
            }
            Event::Tick => {}
        }
    }

    /// Tell the far end how big a terminal it is drawing into.
    pub fn screen_size(&self) -> (u16, u16) {
        (self.cols, self.body_rows())
    }

    /// The owner's own window changed size.
    ///
    /// The live screen is deliberately not resized here: it models the
    /// operator's terminal, and the owner shrinking their window must not
    /// reflow the far end's display. What fits, fits; the rest is clipped.
    pub fn resized(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
    }

    fn body_rows(&self) -> u16 {
        self.rows.saturating_sub(2).max(1)
    }

    fn line(&mut self, text: String) {
        if !self.tty {
            println!("{text}");
        }
        self.transcript.push_back(text);
        while self.transcript.len() > SCROLLBACK {
            self.transcript.pop_front();
        }
    }

    fn write_transcript(&mut self, data: &[u8]) {
        self.pending.push_str(&sanitize(data));
        while let Some(at) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=at).collect();
            self.line(format!("  {}", line.trim_end_matches('\n')));
        }
        // A very long line with no newline in it must not grow without bound.
        if self.pending.len() > 8192 {
            let line = std::mem::take(&mut self.pending);
            self.line(format!("  {line}"));
        }
    }

    fn flush_pending(&mut self) {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.line(format!("  {line}"));
        }
    }

    fn header(&self) -> String {
        let secs = self.started.elapsed().as_secs();
        fit(
            &format!(
                " tether · {} · {} · code {} · {:02}:{:02}",
                self.facts.operator,
                self.facts.relay,
                self.facts.code,
                secs / 60,
                secs % 60
            ),
            self.cols,
        )
    }

    pub fn draw(&mut self) -> io::Result<()> {
        if !self.tty {
            return Ok(());
        }
        if let Ok((cols, rows)) = terminal::size() {
            if (cols, rows) != (self.cols, self.rows) {
                self.resized(cols, rows);
            }
        }
        let mut out = io::stdout();
        let body = self.body_rows();

        queue!(
            out,
            cursor::MoveTo(0, 0),
            terminal::Clear(terminal::ClearType::CurrentLine)
        )?;
        queue!(
            out,
            style::SetAttribute(style::Attribute::Reverse),
            style::Print(self.header()),
            style::SetAttribute(style::Attribute::Reset)
        )?;

        match self.live.as_ref() {
            Some(live) => {
                // Only the parsed grid is drawn. Nothing the far end sent is
                // handed to the owner's terminal to interpret.
                for (i, row) in live
                    .parser
                    .screen()
                    .rows_formatted(0, self.cols)
                    .enumerate()
                {
                    if i as u16 >= body {
                        break;
                    }
                    queue!(
                        out,
                        cursor::MoveTo(0, 1 + i as u16),
                        terminal::Clear(terminal::ClearType::CurrentLine)
                    )?;
                    out.write_all(&row)?;
                    queue!(out, style::SetAttribute(style::Attribute::Reset))?;
                }
            }
            None => {
                let start = self.transcript.len().saturating_sub(body as usize);
                for i in 0..body {
                    let text = self
                        .transcript
                        .get(start + i as usize)
                        .map(|l| fit(l, self.cols))
                        .unwrap_or_default();
                    queue!(
                        out,
                        cursor::MoveTo(0, 1 + i),
                        terminal::Clear(terminal::ClearType::CurrentLine),
                        style::Print(text)
                    )?;
                }
            }
        }

        queue!(
            out,
            cursor::MoveTo(0, self.rows.saturating_sub(1)),
            terminal::Clear(terminal::ClearType::CurrentLine),
            style::SetAttribute(style::Attribute::Reverse),
            style::Print(fit(
                " ctrl-c cuts the session and ends everything running in it ",
                self.cols
            )),
            style::SetAttribute(style::Attribute::Reset)
        )?;
        out.flush()
    }

    /// Put the terminal back exactly as it was found.
    ///
    /// Also runs from `Drop`, because a client that leaves a terminal in raw
    /// mode with no cursor has left something behind — and leaving nothing
    /// behind is the whole claim.
    pub fn restore(&mut self) {
        if !self.tty {
            return;
        }
        self.tty = false;
        let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen, cursor::Show);
        let _ = terminal::disable_raw_mode();
        for line in &self.transcript {
            println!("{line}");
        }
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        self.restore();
    }
}

/// What the owner pressed, reduced to the only thing this client acts on.
pub enum Key {
    Cut,
}

/// Watch the keyboard on a thread, because crossterm's reader blocks.
pub fn watch_keys(tx: tokio::sync::mpsc::Sender<Key>) {
    std::thread::spawn(move || loop {
        match event::poll(std::time::Duration::from_millis(200)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => break,
        }
        let Ok(event::Event::Key(key)) = event::read() else {
            continue;
        };
        let cut = matches!(key.code, event::KeyCode::Char('c'))
            && key.modifiers.contains(event::KeyModifiers::CONTROL);
        if cut && tx.blocking_send(Key::Cut).is_err() {
            break;
        }
    });
}

/// Pad or cut a line to the terminal's width.
fn fit(text: &str, cols: u16) -> String {
    let cols = cols as usize;
    let mut out: String = text.chars().take(cols).collect();
    let len = out.chars().count();
    if len < cols {
        out.push_str(&" ".repeat(cols - len));
    }
    out
}

/// Make a byte stream safe to print into the owner's own terminal.
///
/// Escape sequences are shown, not obeyed. On a tool that promises the owner
/// sees everything, output that can reposition the cursor is output that can
/// hide itself.
fn sanitize(data: &[u8]) -> String {
    String::from_utf8_lossy(data)
        .chars()
        .filter(|c| *c != '\r')
        .map(|c| match c {
            '\n' | '\t' => c,
            c if (c as u32) < 0x20 || c == '\u{7f}' => '·',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_sequences_are_shown_rather_than_obeyed() {
        let clear_screen = b"\x1b[2J\x1b[Hgone?";
        let text = sanitize(clear_screen);
        assert!(!text.contains('\x1b'), "{text:?}");
        assert!(text.contains("gone?"));
        assert!(text.contains('·'));
    }

    #[test]
    fn tabs_and_newlines_survive_but_carriage_returns_do_not() {
        assert_eq!(sanitize(b"a\tb\nc\r\n"), "a\tb\nc\n");
    }

    #[test]
    fn invalid_utf8_does_not_lose_the_rest_of_the_line() {
        assert!(sanitize(b"ok \xff\xfe done").contains("done"));
    }

    #[test]
    fn a_line_is_padded_and_cut_to_the_width() {
        assert_eq!(fit("ab", 5), "ab   ");
        assert_eq!(fit("abcdef", 3), "abc");
        assert_eq!(fit("", 0), "");
    }
}
