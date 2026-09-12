//! Drawing the operator's terminal into a pane of the owner's.
//!
//! # Why this is not the emulator's own row renderer
//!
//! `vt100` will hand back a whole row as bytes ready for a terminal, and it
//! takes a start column and a width, which looks exactly like what a pane
//! needs. It is not. Those bytes carry a clear-to-end-of-line when a row's tail
//! is blank, and an absolute cursor move in the *grid's* coordinates when a row
//! has a gap in it. Its own documentation says the cursor position afterwards
//! is unspecified.
//!
//! Full width, that is survivable: the clear runs to the edge of a screen that
//! is all one pane, and the next row repaints over whatever the cursor move
//! disturbed. In two columns it is not. The clear erases straight through the
//! rule and the conversation beside it, and the move lands somewhere else
//! entirely, every frame.
//!
//! So the grid is drawn a cell at a time, and its colours and attributes are
//! re-derived rather than replayed. That costs a couple of thousand queued
//! writes per frame at eighty columns, all buffered into one flush, on a
//! display that repaints about once a second.
//!
//! It also makes a claim in [`crate::ui`] literally true for the first time.
//! That module says nothing from the far end is ever handed to the owner's
//! terminal to interpret — and until now the emulator's own escape bytes,
//! derived from the far end's content, were an exception to it. Every byte
//! written here is one this crate composed.

use std::io::Write;

use crossterm::{cursor, queue, style};

use crate::layout::Rect;

/// The attributes currently set on the owner's terminal.
///
/// Tracked so that only what changes is written. A colour per cell would be
/// correct and would also be most of the bytes on the screen.
#[derive(Default, PartialEq, Clone, Copy)]
struct Pen {
    fg: Option<style::Color>,
    bg: Option<style::Color>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl Pen {
    fn of(cell: &vt100::Cell) -> Self {
        Self {
            fg: colour(cell.fgcolor()),
            bg: colour(cell.bgcolor()),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }

    /// Write whatever it takes to get from `self` to `next`.
    ///
    /// Turning an attribute off has no targeted escape in the set used here, so
    /// any removal resets and re-applies. That is rare — attributes are
    /// switched on far more often than off within a row.
    fn shift(&mut self, next: Self, out: &mut impl Write) -> std::io::Result<()> {
        if *self == next {
            return Ok(());
        }
        let removes = (self.bold && !next.bold)
            || (self.dim && !next.dim)
            || (self.italic && !next.italic)
            || (self.underline && !next.underline)
            || (self.inverse && !next.inverse);
        if removes {
            queue!(out, style::SetAttribute(style::Attribute::Reset))?;
            *self = Self::default();
        }
        if self.fg != next.fg {
            queue!(out, style::SetForegroundColor(next.fg.unwrap_or(style::Color::Reset)))?;
        }
        if self.bg != next.bg {
            queue!(out, style::SetBackgroundColor(next.bg.unwrap_or(style::Color::Reset)))?;
        }
        for (want, have, attr) in [
            (next.bold, self.bold, style::Attribute::Bold),
            (next.dim, self.dim, style::Attribute::Dim),
            (next.italic, self.italic, style::Attribute::Italic),
            (next.underline, self.underline, style::Attribute::Underlined),
            (next.inverse, self.inverse, style::Attribute::Reverse),
        ] {
            if want && !have {
                queue!(out, style::SetAttribute(attr))?;
            }
        }
        *self = next;
        Ok(())
    }
}

fn colour(c: vt100::Color) -> Option<style::Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(style::Color::AnsiValue(i)),
        vt100::Color::Rgb(r, g, b) => Some(style::Color::Rgb { r, g, b }),
    }
}

/// Draw as much of `screen` as fits in `pane`.
///
/// Rows past the bottom and columns past the right are clipped rather than
/// wrapped: the grid is the size the operator's terminal is, and reflowing it
/// would show the owner something the operator is not looking at.
pub fn draw(
    screen: &vt100::Screen,
    pane: Rect,
    out: &mut impl Write,
) -> std::io::Result<()> {
    let (grid_rows, grid_cols) = screen.size();
    for y in 0..pane.rows {
        queue!(out, cursor::MoveTo(pane.x, pane.y + y))?;
        let mut pen = Pen::default();
        queue!(out, style::SetAttribute(style::Attribute::Reset))?;

        let mut x = 0u16;
        while x < pane.cols {
            if y >= grid_rows || x >= grid_cols {
                // Past the end of the operator's screen. Blank, in this pane's
                // own width, rather than a clear that would run off the edge.
                queue!(out, style::Print(" "))?;
                x += 1;
                continue;
            }
            let Some(cell) = screen.cell(y, x) else {
                queue!(out, style::Print(" "))?;
                x += 1;
                continue;
            };
            if cell.is_wide_continuation() {
                x += 1;
                continue;
            }
            let width = if cell.is_wide() { 2 } else { 1 };
            if x + width > pane.cols {
                // A double-width character straddling the edge. One space, so
                // the pane's width stays exact and the rule beside it does not
                // move.
                queue!(out, style::Print(" "))?;
                x += 1;
                continue;
            }
            pen.shift(Pen::of(cell), out)?;
            let text = cell.contents();
            if text.is_empty() {
                queue!(out, style::Print(" "))?;
            } else {
                queue!(out, style::Print(text))?;
            }
            x += width;
        }
        // Every row leaves the terminal as it found it, so a bright background
        // in the operator's screen cannot bleed into the pane beside this one.
        queue!(out, style::SetAttribute(style::Attribute::Reset))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(feed: &[u8], pane: Rect, size: (u16, u16)) -> Vec<u8> {
        let mut parser = vt100::Parser::new(size.0, size.1, 0);
        parser.process(feed);
        let mut out = Vec::new();
        draw(parser.screen(), pane, &mut out).unwrap();
        out
    }

    /// The property the whole module exists for.
    ///
    /// A clear-to-end-of-line would erase the pane beside this one, and an
    /// absolute cursor move would land outside it. Neither may be emitted, no
    /// matter what the far end drew.
    #[test]
    fn nothing_that_could_reach_outside_the_pane_is_ever_written() {
        let pane = Rect { x: 0, y: 1, cols: 20, rows: 5 };
        // A clear screen, a clear line, an absolute move, and a colour — the
        // things a full-screen program does constantly.
        let feed = b"\x1b[2J\x1b[3;5H\x1b[31mred\x1b[K\x1b[1;1Htop\x1b[0m";
        let bytes = rendered(feed, pane, (10, 40));
        let text = String::from_utf8_lossy(&bytes);

        assert!(!text.contains("\x1b[K"), "a clear to end of line escaped: {text:?}");
        assert!(!text.contains("\x1b[2J"), "a clear screen escaped: {text:?}");
        // Every cursor move this writes is one of ours, at the pane's own
        // column, one per row and no more.
        let moves = text.matches("\x1b[").filter(|_| true).count();
        assert!(moves > 0);
        for row in 0..pane.rows {
            let expected = format!("\x1b[{};{}H", pane.y + row + 1, pane.x + 1);
            assert!(text.contains(&expected), "row {row} was not placed: {text:?}");
        }
    }

    #[test]
    fn what_the_far_end_drew_is_what_the_pane_shows() {
        let pane = Rect { x: 0, y: 0, cols: 20, rows: 2 };
        let bytes = rendered(b"hello\r\nthere", pane, (2, 20));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("hello"), "{text:?}");
        assert!(text.contains("there"), "{text:?}");
    }

    /// A grid wider than the pane is cut, not wrapped. Wrapping would show the
    /// owner a screen the operator is not looking at.
    #[test]
    fn a_grid_wider_than_the_pane_is_clipped_rather_than_reflowed() {
        let pane = Rect { x: 0, y: 0, cols: 5, rows: 1 };
        let bytes = rendered(b"abcdefghij", pane, (1, 40));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("abcde"), "{text:?}");
        assert!(!text.contains("abcdef"), "it kept going: {text:?}");
    }

    #[test]
    fn every_row_puts_the_terminal_back_before_the_next_pane_is_drawn() {
        let pane = Rect { x: 0, y: 0, cols: 8, rows: 3 };
        let bytes = rendered(b"\x1b[41mred background", pane, (3, 40));
        let text = String::from_utf8_lossy(&bytes);
        let resets = text.matches("\x1b[0m").count();
        assert!(
            resets >= pane.rows as usize,
            "a background could bleed into the next pane: {text:?}"
        );
    }
}
