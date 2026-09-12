//! Where each part of the owner's screen is, this frame.
//!
//! A pure function of the terminal's size, which is the only reason the layout
//! is testable at all: everything else in the display writes bytes.
//!
//! # The command pane never yields
//!
//! The prompt the owner agreed to says they will see everything the operator
//! does while it happens. So when the window is too small to carry both, it is
//! the conversation that gives way — first to a strip, then to a single line to
//! type on, then to nothing. The command pane has a floor and the chat has a
//! ceiling, and they are not the same kind of limit.
//!
//! Nothing here is loses anything when the chat pane disappears. Every spoken
//! line is in the transcript and in the record as well, so the pane is a view
//! onto a subset rather than the only place a message exists. That is what
//! makes collapsing it safe.
//!
//! # The far end cannot change any of this
//!
//! No frame the operator can send affects the split. Everything else on this
//! screen is driven by the other end; this is the owner's own window.

/// A rectangle of the owner's terminal, in absolute screen coordinates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
}

/// How the screen is arranged at this size.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shape {
    /// Commands on the left, conversation on the right.
    Side,
    /// Commands above, a strip of conversation below.
    Stacked,
    /// Commands only. Too small to type in.
    NoChat,
}

#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub header: Rect,
    /// Commands and the live terminal.
    pub work: Rect,
    /// The column the vertical rule is drawn in, when there is one.
    pub rule: Option<u16>,
    pub chat: Option<Rect>,
    /// The line being typed.
    pub input: Option<Rect>,
    pub footer: Rect,
    pub shape: Shape,
}

/// Narrower than this and a conversation is a column of single words.
const CHAT_MIN: u16 = 20;

/// Wider than this and the chat is taking space the commands could use.
const CHAT_MAX: u16 = 36;

/// Narrower than this and a command line stops being readable.
const WORK_MIN: u16 = 40;

/// Below this there is no room to both work and talk.
const SIDE_MIN_COLS: u16 = WORK_MIN + 1 + CHAT_MIN;

pub fn split(cols: u16, rows: u16) -> Layout {
    let header = Rect { x: 0, y: 0, cols, rows: 1 };
    let footer = Rect {
        x: 0,
        y: rows.saturating_sub(1),
        cols,
        rows: 1,
    };
    let body_rows = rows.saturating_sub(2).max(1);
    let full = Rect { x: 0, y: 1, cols, rows: body_rows };

    if rows < 8 || cols < 24 {
        return Layout {
            header,
            work: full,
            rule: None,
            chat: None,
            input: None,
            footer,
            shape: Shape::NoChat,
        };
    }

    if cols >= SIDE_MIN_COLS {
        let mut chat_cols = (cols / 4).clamp(CHAT_MIN, CHAT_MAX);
        chat_cols = chat_cols.min(cols.saturating_sub(1 + WORK_MIN));
        let work_cols = cols - 1 - chat_cols;
        let rule = work_cols;
        let chat_x = work_cols + 1;
        // The last row of the column is where they type; the rest is what was
        // said. One blank row between, when there is room for one.
        let input_y = rows.saturating_sub(2);
        return Layout {
            header,
            work: Rect { x: 0, y: 1, cols: work_cols, rows: body_rows },
            rule: Some(rule),
            chat: Some(Rect {
                x: chat_x,
                y: 1,
                cols: chat_cols,
                rows: body_rows.saturating_sub(2).max(1),
            }),
            input: Some(Rect { x: chat_x, y: input_y, cols: chat_cols, rows: 1 }),
            footer,
            shape: Shape::Side,
        };
    }

    // Too narrow for two columns. The conversation becomes a strip along the
    // bottom, and shrinks to just the line being typed when even that is tight.
    let strip = if rows >= 10 { 4 } else { 2 };
    let work_rows = body_rows.saturating_sub(strip).max(1);
    let strip_y = 1 + work_rows;
    Layout {
        header,
        work: Rect { x: 0, y: 1, cols, rows: work_rows },
        rule: None,
        chat: (strip > 2).then(|| Rect {
            x: 0,
            y: strip_y + 1,
            cols,
            rows: strip - 2,
        }),
        input: Some(Rect { x: 0, y: rows.saturating_sub(2), cols, rows: 1 }),
        footer,
        shape: Shape::Stacked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sizes a person actually has, and what each one gives them.
    #[test]
    fn the_command_pane_has_a_floor_and_the_chat_has_a_ceiling() {
        for (cols, work, chat) in [
            (200, 163, 36),
            (120, 89, 30),
            (100, 74, 25),
            (80, 59, 20),
            (61, 40, 20),
        ] {
            let l = split(cols, 40);
            assert_eq!(l.shape, Shape::Side, "at {cols} columns");
            assert_eq!(l.work.cols, work, "work at {cols}");
            assert_eq!(l.chat.unwrap().cols, chat, "chat at {cols}");
            assert!(l.work.cols >= WORK_MIN);
            assert!(l.chat.unwrap().cols <= CHAT_MAX);
        }
    }

    #[test]
    fn the_panes_never_overlap_and_the_rule_sits_between_them() {
        for cols in [61u16, 80, 100, 137, 200] {
            let l = split(cols, 30);
            let work = l.work;
            let chat = l.chat.unwrap();
            let rule = l.rule.unwrap();
            assert_eq!(work.x + work.cols, rule, "the rule follows the work pane");
            assert_eq!(rule + 1, chat.x, "and the chat follows the rule");
            assert_eq!(chat.x + chat.cols, cols, "and the chat reaches the edge");
        }
    }

    #[test]
    fn a_narrow_window_stacks_rather_than_squeezing_the_commands() {
        let l = split(60, 24);
        assert_eq!(l.shape, Shape::Stacked);
        assert_eq!(l.work.cols, 60, "the commands keep the whole width");
        assert!(l.chat.is_some());
        assert!(l.input.is_some(), "there is still somewhere to type");
    }

    #[test]
    fn a_short_window_keeps_the_line_to_type_on_and_drops_the_rest() {
        let l = split(50, 9);
        assert_eq!(l.shape, Shape::Stacked);
        assert!(l.chat.is_none(), "no room to show what was said");
        assert!(l.input.is_some(), "but still room to say something");
    }

    /// The floor. What is left is exactly what the display was before any of
    /// this, which is the property that makes collapsing safe to do.
    #[test]
    fn a_window_too_small_to_talk_in_is_the_display_as_it_always_was() {
        for (cols, rows) in [(80, 7), (20, 40), (10, 5)] {
            let l = split(cols, rows);
            assert_eq!(l.shape, Shape::NoChat, "at {cols}x{rows}");
            assert!(l.chat.is_none());
            assert!(l.input.is_none());
            assert!(l.rule.is_none());
            assert_eq!(l.work.cols, cols, "the commands take everything");
        }
    }

    #[test]
    fn nothing_is_ever_drawn_off_the_bottom_or_the_right() {
        for cols in [10u16, 24, 40, 61, 80, 200] {
            for rows in [3u16, 5, 8, 10, 24, 60] {
                let l = split(cols, rows);
                for rect in [Some(l.work), l.chat, l.input, Some(l.footer)]
                    .into_iter()
                    .flatten()
                {
                    assert!(rect.x + rect.cols <= cols, "{cols}x{rows}: {rect:?}");
                    assert!(rect.y + rect.rows <= rows.max(1), "{cols}x{rows}: {rect:?}");
                }
                if let Some(rule) = l.rule {
                    assert!(rule < cols);
                }
            }
        }
    }
}
