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

use crate::layout;
use crate::log::Log;
use crate::screen;

/// How much scrollback the transcript keeps.
const SCROLLBACK: usize = 2000;

/// How many spoken lines the conversation pane keeps.
const CHAT_SCROLLBACK: usize = 200;

/// The longest spoken line that will be drawn.
///
/// A frame may carry a megabyte. One line of conversation is not a megabyte,
/// and a display that tried to render one would stop being a display.
const MAX_SAID: usize = 4096;

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
    /// The record both people agreed to. Hung off `line` below rather than off
    /// the wire, so "what is in the log is what you saw" is true because there
    /// is one funnel, not because somebody checked.
    log: Option<Log>,
    /// The line the owner is typing.
    compose: Compose,
    /// The conversation, as its own list. Every line here is also in the
    /// transcript and the record, so a display too narrow to carry this pane
    /// loses nothing — which is what lets the layout collapse freely.
    chat: VecDeque<Chat>,
}

/// One thing somebody said.
struct Chat {
    from_operator: bool,
    text: String,
}

impl Ui {
    pub fn new(facts: Facts, log: Option<Log>) -> io::Result<Self> {
        let tty = std::io::IsTerminal::is_terminal(&io::stdout());
        let (cols, rows) = measure();
        let mut ui = Self {
            facts,
            started: Instant::now(),
            transcript: VecDeque::new(),
            pending: String::new(),
            live: None,
            tty,
            cols,
            rows,
            log,
            compose: Compose::default(),
            chat: VecDeque::new(),
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
                let text = say_text(&text);
                let who = if from_operator { "them" } else { "you" };
                self.line(format!("{who}: {text}"));
                self.chat.push_back(Chat {
                    from_operator,
                    text,
                });
                while self.chat.len() > CHAT_SCROLLBACK {
                    self.chat.pop_front();
                }
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

    /// How big a terminal the far end should draw into.
    ///
    /// The work pane, less the row the transcript keeps. This had no callers
    /// for the whole life of the tool: the operator hardcoded a size and the
    /// owner clipped whatever arrived, so a grid wider than their window was
    /// silently cut and nobody was told.
    ///
    /// **The owner's pane is the authority, and that is a reversal.** The
    /// display used to refuse to resize the emulator when this window changed,
    /// on the grounds that the grid models the operator's terminal rather than
    /// the owner's. That reasoning held while nothing was negotiated. Now that
    /// the size is asked for, the owner's pane is what the operator is told to
    /// draw into — because a grid the owner cannot see all of breaks the
    /// sentence they consented to, and the operator is the one who can afford
    /// to be letterboxed.
    pub fn viewport(&self) -> (u16, u16) {
        let plan = layout::split(self.cols, self.rows);
        (
            plan.work.cols.max(20),
            plan.work.rows.saturating_sub(1).max(5),
        )
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

    fn line(&mut self, text: String) {
        if !self.tty {
            println!("{text}");
        }
        if let Some(log) = self.log.as_mut() {
            log.line(&text);
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
        let (cols, rows) = measure();
        if (cols, rows) != (self.cols, self.rows) {
            self.resized(cols, rows);
        }
        let plan = layout::split(self.cols, self.rows);
        let mut out = io::stdout();
        // Hidden for the whole repaint, so the caret does not skate across the
        // screen once a second on its way to the line being typed.
        queue!(out, cursor::Hide)?;

        let header = self.header();
        region(&mut out, plan.header, 0, &header, true)?;

        self.draw_work(&mut out, plan.work)?;

        if let Some(rule) = plan.rule {
            for y in plan.work.y..plan.work.y + plan.work.rows {
                queue!(
                    out,
                    cursor::MoveTo(rule, y),
                    style::SetAttribute(style::Attribute::Dim),
                    style::Print("|"),
                    style::SetAttribute(style::Attribute::Reset)
                )?;
            }
        }

        if let Some(pane) = plan.chat {
            self.draw_chat(&mut out, pane)?;
        }

        let footer = fit(
            match plan.shape {
                layout::Shape::NoChat => " ctrl-c cuts the session and ends everything running in it ",
                _ => " type to reply · enter sends · esc clears · ctrl-c cuts the session ",
            },
            self.cols,
        );
        region(&mut out, plan.footer, 0, &footer, true)?;

        // Last, so the caret is left where the owner is typing.
        if let Some(input) = plan.input {
            let caret = self.draw_input(&mut out, input)?;
            queue!(out, cursor::MoveTo(caret, input.y), cursor::Show)?;
        }
        out.flush()
    }

    /// The pane the operator's work is in.
    ///
    /// When a terminal is live it takes its own height at the top, and the
    /// transcript fills whatever rows are left. That last row matters: it is
    /// what stops a command's output going invisible while a terminal is open,
    /// which is what the display used to do.
    fn draw_work(&self, out: &mut impl Write, pane: layout::Rect) -> io::Result<()> {
        let grid_rows = match self.live.as_ref() {
            Some(live) => {
                let (rows, _) = live.parser.screen().size();
                let top = rows.min(pane.rows);
                screen::draw(
                    live.parser.screen(),
                    layout::Rect { rows: top, ..pane },
                    out,
                )?;
                top
            }
            None => 0,
        };

        let rows = pane.rows.saturating_sub(grid_rows);
        if rows == 0 {
            return Ok(());
        }
        let start = self.transcript.len().saturating_sub(rows as usize);
        for i in 0..rows {
            let text = self
                .transcript
                .get(start + i as usize)
                .map(|l| l.as_str())
                .unwrap_or("");
            region(
                out,
                layout::Rect { y: pane.y + grid_rows + i, rows: 1, ..pane },
                0,
                &fit(text, pane.cols),
                false,
            )?;
        }
        Ok(())
    }

    fn draw_chat(&self, out: &mut impl Write, pane: layout::Rect) -> io::Result<()> {
        let lines = self.chat_lines(pane.cols);
        let start = lines.len().saturating_sub(pane.rows as usize);
        for i in 0..pane.rows {
            let text = lines
                .get(start + i as usize)
                .map(|l| l.as_str())
                .unwrap_or("");
            region(
                out,
                layout::Rect { y: pane.y + i, rows: 1, ..pane },
                0,
                &fit(text, pane.cols),
                false,
            )?;
        }
        Ok(())
    }

    /// The line being typed, and where the caret sits in it.
    ///
    /// A window on the composed line rather than the whole of it, always
    /// containing the caret, so a long reply scrolls sideways rather than
    /// spilling into the pane above.
    fn draw_input(&self, out: &mut impl Write, pane: layout::Rect) -> io::Result<u16> {
        const PROMPT: &str = "> ";
        let room = pane.cols.saturating_sub(PROMPT.len() as u16 + 1) as usize;
        let before: String = self.compose.text[..self.compose.caret].to_string();
        let shown_before = tail_of_width(&before, room);
        let rest = &self.compose.text[self.compose.caret..];
        let mut line = String::from(PROMPT);
        line.push_str(shown_before);
        line.push_str(&fit(rest, room.saturating_sub(width_of(shown_before)) as u16));

        region(out, pane, 0, &fit(&line, pane.cols), false)?;
        Ok(pane.x + PROMPT.len() as u16 + width_of(shown_before) as u16)
    }

    /// The conversation, wrapped to a width, oldest first.
    fn chat_lines(&self, cols: u16) -> Vec<String> {
        let width = cols.max(4) as usize;
        let mut out = Vec::new();
        for said in &self.chat {
            let who = if said.from_operator { "them: " } else { "you: " };
            let mut first = true;
            let mut rest = said.text.as_str();
            while !rest.is_empty() || first {
                let lead = if first { who } else { "  " };
                let room = width.saturating_sub(width_of(lead)).max(1);
                let take = tail_split(rest, room);
                out.push(format!("{lead}{}", &rest[..take]));
                rest = rest[take..].trim_start_matches(' ');
                first = false;
                if take == 0 {
                    break;
                }
            }
        }
        out
    }

    /// Put the terminal back exactly as it was found.
    ///
    /// Also runs from `Drop`, because a client that leaves a terminal in raw
    /// mode with no cursor has left something behind — and leaving nothing
    /// behind is the whole claim.
    pub fn restore(&mut self) {
        // Closed first, so the record is complete even if the replay below
        // never happens. It is also the half that matters if this is running
        // from `Drop` because something panicked.
        let kept = self.log.as_mut().map(|log| {
            log.finish();
            log.path().display().to_string()
        });
        self.log = None;

        if self.tty {
            self.tty = false;
            let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen, cursor::Show);
            let _ = terminal::disable_raw_mode();
            for line in &self.transcript {
                println!("{line}");
            }
        }
        // The same path the prompt named, as the last thing on their screen.
        // First and last, so it is not something they have to have remembered.
        if let Some(path) = kept {
            println!("  a record of this session is in {path}");
        }
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        self.restore();
    }
}

/// The line the owner is typing.
///
/// Never logged and never sent until they press enter. Half a typed line is not
/// something either person said, and a half-typed password is exactly the kind
/// of thing that ends up in one.
#[derive(Default)]
struct Compose {
    text: String,
    /// Where the caret is, as a byte index into `text`.
    caret: usize,
}

impl Compose {
    fn insert(&mut self, c: char) {
        if self.text.chars().count() >= 2000 {
            return;
        }
        self.text.insert(self.caret, c);
        self.caret += c.len_utf8();
    }

    fn backspace(&mut self) {
        let Some(prev) = self.text[..self.caret].chars().next_back() else {
            return;
        };
        self.caret -= prev.len_utf8();
        self.text.remove(self.caret);
    }

    fn delete(&mut self) {
        if self.caret < self.text.len() {
            self.text.remove(self.caret);
        }
    }

    fn left(&mut self) {
        if let Some(prev) = self.text[..self.caret].chars().next_back() {
            self.caret -= prev.len_utf8();
        }
    }

    fn right(&mut self) {
        if let Some(next) = self.text[self.caret..].chars().next() {
            self.caret += next.len_utf8();
        }
    }

    /// Delete back to the start of the word before the caret.
    fn rub_word(&mut self) {
        let head = &self.text[..self.caret];
        let trimmed = head.trim_end_matches(' ');
        let cut = trimmed.rfind(' ').map(|i| i + 1).unwrap_or(0);
        self.text.replace_range(cut..self.caret, "");
        self.caret = cut;
    }

    fn take(&mut self) -> String {
        self.caret = 0;
        std::mem::take(&mut self.text)
    }

    fn clear(&mut self) {
        self.caret = 0;
        self.text.clear();
    }
}

/// What one keystroke meant to this client.
pub enum Typed {
    /// Nothing worth redrawing for.
    Nothing,
    /// The composed line changed; the screen should be repainted.
    Changed,
    /// End the session, now.
    Cut,
    /// Send this to the other person.
    Send(String),
}

/// The owner's keyboard, as a stream this side can stop reading.
///
/// It used to be a detached thread that polled, recognised the one key that
/// cuts a session and discarded every other. That thread was never joined and
/// had no way to be stopped, so it went on consuming keystrokes after the
/// display had torn down — taking them from the shell the owner was handed
/// back. Dropping a stream ends it, which is the whole reason for the change.
pub fn keys() -> event::EventStream {
    event::EventStream::new()
}

impl Ui {
    /// What one terminal event meant.
    pub fn typed(&mut self, ev: event::Event) -> Typed {
        let event::Event::Key(key) = ev else {
            return Typed::Nothing;
        };
        // Windows reports a press and a release for every key. Without this,
        // every character the owner types appears twice — and it appears once
        // on the machine this was written on, which is how it would have been
        // missed.
        if key.kind != event::KeyEventKind::Press {
            return Typed::Nothing;
        }
        let control = key.modifiers.contains(event::KeyModifiers::CONTROL);

        // Unconditional, whatever is half-typed. The tempting alternative is
        // for the first press to clear the line and the second to cut, which
        // trades this tool's panic button for a small convenience. The panic
        // button is a safety property; `esc` clears the line instead.
        if control && matches!(key.code, event::KeyCode::Char('c')) {
            return Typed::Cut;
        }

        match key.code {
            event::KeyCode::Char('u') if control => {
                self.compose.clear();
                Typed::Changed
            }
            event::KeyCode::Char('w') if control => {
                self.compose.rub_word();
                Typed::Changed
            }
            event::KeyCode::Char(c) if !control => {
                self.compose.insert(c);
                Typed::Changed
            }
            event::KeyCode::Backspace => {
                self.compose.backspace();
                Typed::Changed
            }
            event::KeyCode::Delete => {
                self.compose.delete();
                Typed::Changed
            }
            event::KeyCode::Left => {
                self.compose.left();
                Typed::Changed
            }
            event::KeyCode::Right => {
                self.compose.right();
                Typed::Changed
            }
            event::KeyCode::Home => {
                self.compose.caret = 0;
                Typed::Changed
            }
            event::KeyCode::End => {
                self.compose.caret = self.compose.text.len();
                Typed::Changed
            }
            event::KeyCode::Esc => {
                self.compose.clear();
                Typed::Changed
            }
            event::KeyCode::Enter => {
                let line = self.compose.take();
                if line.trim().is_empty() {
                    Typed::Changed
                } else {
                    Typed::Send(line)
                }
            }
            _ => Typed::Nothing,
        }
    }
}

/// Pad or cut a line to the terminal's width.
/// How big the owner's terminal is, with a usable answer when it will not say.
///
/// A pseudo-terminal whose window size was never set reports zero, and a
/// zero-width display draws nothing at all. That is not a small screen, it is a
/// blank one, and it looks exactly like a client that failed to start. Seen for
/// real on a session opened without a window size, where the whole display
/// repainted two empty rows about once a second.
fn measure() -> (u16, u16) {
    match terminal::size() {
        Ok((cols, rows)) if cols > 0 && rows > 0 => (cols, rows),
        _ => (80, 24),
    }
}

/// Write one line into one region, and put the terminal back afterwards.
///
/// The only way anything but the grid reaches the screen. A region pads itself
/// to its exact width rather than clearing the line, because clearing spans the
/// whole physical row and would erase whatever is beside it.
fn region(
    out: &mut impl Write,
    rect: layout::Rect,
    row: u16,
    text: &str,
    reversed: bool,
) -> io::Result<()> {
    queue!(out, cursor::MoveTo(rect.x, rect.y + row))?;
    if reversed {
        queue!(out, style::SetAttribute(style::Attribute::Reverse))?;
    }
    queue!(
        out,
        style::Print(text),
        style::SetAttribute(style::Attribute::Reset)
    )
}

/// How many columns a string occupies on screen.
fn width_of(text: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    text.width()
}

/// The longest tail of `text` that fits in `room` columns, on a char boundary.
fn tail_of_width(text: &str, room: usize) -> &str {
    use unicode_width::UnicodeWidthChar;
    let mut used = 0;
    let mut start = text.len();
    for (i, c) in text.char_indices().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > room {
            break;
        }
        used += w;
        start = i;
    }
    &text[start..]
}

/// How many bytes of `text` fit in `room` columns, preferring a word boundary.
fn tail_split(text: &str, room: usize) -> usize {
    use unicode_width::UnicodeWidthChar;
    let mut used = 0;
    let mut end = 0;
    for (i, c) in text.char_indices() {
        let w = c.width().unwrap_or(0);
        if used + w > room {
            // Back up to the last space, unless that would take everything.
            if let Some(space) = text[..i].rfind(' ') {
                if space > 0 {
                    return space;
                }
            }
            return end;
        }
        used += w;
        end = i + c.len_utf8();
    }
    end
}

/// One spoken line, safe to print and bounded.
///
/// Everything else arriving from the far end already goes through `sanitize`
/// before it reaches the screen, and this did not: a `Say` was printed as it
/// arrived. So the operator could clear the owner's screen, move their cursor
/// or redraw it as something else, in a client whose own documentation says
/// nothing from the far end is ever executed by the owner's terminal.
///
/// Applied in both directions, because the owner pasting a control character
/// must not be able to corrupt their own display either.
fn say_text(text: &str) -> String {
    let mut out = sanitize(text.as_bytes());
    out = out.replace(['\n', '\t'], " ");
    out.truncate(MAX_SAID);
    out
}

fn fit(text: &str, cols: u16) -> String {
    use unicode_width::UnicodeWidthChar;
    let cols = cols as usize;
    let (mut out, mut used) = (String::new(), 0usize);
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > cols {
            // A double-width character that would straddle the edge becomes a
            // space, so the region's width stays exact and whatever is drawn
            // beside it does not move. Counting characters instead of columns
            // is what made that shift before there was anything beside it.
            if used < cols {
                out.push(' ');
                used += 1;
            }
            break;
        }
        out.push(c);
        used += w;
    }
    out.push_str(&" ".repeat(cols.saturating_sub(used)));
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

    /// Measured in columns rather than characters. Counting characters put the
    /// rule and the pane beside it in a different place on every row that had
    /// a wide glyph in it.
    #[test]
    fn a_wide_character_costs_two_columns_and_never_straddles_the_edge() {
        assert_eq!(width_of("ab"), 2);
        assert_eq!(width_of("日本"), 4);

        assert_eq!(fit("日本", 4), "日本");
        // Three columns cannot hold two double-width glyphs. The second becomes
        // a space so the width is still exactly three.
        assert_eq!(fit("日本", 3), "日 ");
        assert_eq!(width_of(&fit("日本", 3)), 3);
        // A combining mark attaches to what came before and costs nothing.
        assert_eq!(width_of("e\u{301}"), 1);
    }

    /// The defect this found in shipped code: a spoken line went to the screen
    /// unsanitised, in a client whose own documentation says nothing from the
    /// far end is ever obeyed by the owner's terminal.
    #[test]
    fn a_spoken_line_is_shown_rather_than_obeyed() {
        let attack = "\x1b[2Jgone\x1b[1;1H";
        let shown = say_text(attack);
        assert!(!shown.contains('\x1b'), "{shown:?}");
        assert!(shown.contains("gone"));
        // One line, whatever arrived. A newline in the middle of a message
        // would push everything below it down by a row.
        assert!(!say_text("two\nlines").contains('\n'));
        // And bounded, because a frame may carry a megabyte and a line of
        // conversation is not a megabyte.
        assert!(say_text(&"x".repeat(100_000)).len() <= MAX_SAID);
    }

    fn press(code: event::KeyCode) -> event::Event {
        event::Event::Key(event::KeyEvent::new(code, event::KeyModifiers::NONE))
    }

    fn control(c: char) -> event::Event {
        event::Event::Key(event::KeyEvent::new(
            event::KeyCode::Char(c),
            event::KeyModifiers::CONTROL,
        ))
    }

    fn typing() -> Ui {
        Ui {
            facts: Facts {
                operator: "awm as tony on altair".into(),
                relay: "example/tether".into(),
                code: "acre anchor kettle".into(),
            },
            started: Instant::now(),
            transcript: VecDeque::new(),
            pending: String::new(),
            live: None,
            tty: false,
            cols: 100,
            rows: 30,
            log: None,
            compose: Compose::default(),
            chat: VecDeque::new(),
        }
    }

    #[test]
    fn a_line_is_composed_and_sent_on_enter() {
        let mut ui = typing();
        for c in "hello".chars() {
            assert!(matches!(ui.typed(press(event::KeyCode::Char(c))), Typed::Changed));
        }
        match ui.typed(press(event::KeyCode::Enter)) {
            Typed::Send(line) => assert_eq!(line, "hello"),
            _ => panic!("enter should send"),
        }
        // And the line is gone, rather than sent twice.
        assert!(matches!(ui.typed(press(event::KeyCode::Enter)), Typed::Changed));
    }

    #[test]
    fn editing_a_line_works_the_way_a_line_works() {
        let mut ui = typing();
        for c in "helo".chars() {
            ui.typed(press(event::KeyCode::Char(c)));
        }
        ui.typed(press(event::KeyCode::Left));
        ui.typed(press(event::KeyCode::Char('l')));
        assert_eq!(ui.compose.text, "hello");

        ui.typed(press(event::KeyCode::End));
        ui.typed(press(event::KeyCode::Backspace));
        assert_eq!(ui.compose.text, "hell");

        ui.typed(press(event::KeyCode::Esc));
        assert_eq!(ui.compose.text, "");
    }

    /// Whatever is half-typed. The tempting alternative is for the first press
    /// to clear the line, which trades this tool's panic button for a
    /// convenience — and the panic button is a safety property.
    #[test]
    fn ctrl_c_cuts_the_session_even_mid_sentence() {
        let mut ui = typing();
        for c in "wait no".chars() {
            ui.typed(press(event::KeyCode::Char(c)));
        }
        assert!(matches!(ui.typed(control('c')), Typed::Cut));
    }

    /// Windows reports a press and a release for every key, and this runs on
    /// Linux, so without the filter every character would double in the field
    /// and nowhere else.
    #[test]
    fn a_key_being_released_is_not_a_key_being_typed() {
        let mut ui = typing();
        let mut release = event::KeyEvent::new(event::KeyCode::Char('a'), event::KeyModifiers::NONE);
        release.kind = event::KeyEventKind::Release;
        ui.typed(event::Event::Key(release));
        assert_eq!(ui.compose.text, "", "a release typed a character");
    }

    #[test]
    fn the_conversation_wraps_into_its_pane_and_says_who_spoke() {
        let mut ui = typing();
        ui.apply(Event::Said {
            from_operator: true,
            text: "going to look at the disk now".into(),
        });
        ui.apply(Event::Said {
            from_operator: false,
            text: "go ahead".into(),
        });

        let lines = ui.chat_lines(20);
        assert!(lines[0].starts_with("them: "), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("you: ")), "{lines:?}");
        for line in &lines {
            assert!(width_of(line) <= 20, "{line:?} is wider than the pane");
        }
        // Nothing is only in this pane: it is in the transcript too, which is
        // what lets the pane collapse on a narrow window without losing a word.
        assert!(ui.transcript.iter().any(|l| l.contains("go ahead")));
    }
}
