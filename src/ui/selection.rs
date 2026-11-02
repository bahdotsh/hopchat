//! Dragging over text to copy it. A selection is a stretch of the screen,
//! so what it copies is what was drawn there.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use unicode_width::UnicodeWidthStr;

/// What the chat draws beside each message's rows.
const GUTTER: &str = "▎ ";

pub struct Selection {
    anchor: Position,
    head: Position,
    /// The pane it started in: it never reaches past this.
    bounds: Rect,
    pub dragged: bool,
    /// Released and waiting for the next draw to copy it.
    pub copy: bool,
}

impl Selection {
    pub fn new(at: Position, bounds: Rect) -> Self {
        Self { anchor: at, head: at, bounds, dragged: false, copy: false }
    }

    pub fn anchor(&self) -> Position {
        self.anchor
    }

    pub fn drag_to(&mut self, at: Position) {
        let last = |start: u16, size: u16| (start + size).saturating_sub(1).max(start);
        self.head = Position {
            x: at.x.clamp(self.bounds.x, last(self.bounds.x, self.bounds.width)),
            y: at.y.clamp(self.bounds.y, last(self.bounds.y, self.bounds.height)),
        };
        self.dragged = true;
    }

    /// Each row it covers, top to bottom: (row, first column, last column).
    /// Like selecting in a text editor, the rows between the ends are whole.
    fn rows(&self) -> impl Iterator<Item = (u16, u16, u16)> + use<> {
        let (start, end) = if (self.anchor.y, self.anchor.x) <= (self.head.y, self.head.x) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        };
        let left = self.bounds.x;
        let right = (left + self.bounds.width).saturating_sub(1);
        (start.y..=end.y).map(move |y| {
            let from = if y == start.y { start.x } else { left };
            let to = if y == end.y { end.x } else { right };
            (y, from, to)
        })
    }

    pub fn paint(&self, buffer: &mut Buffer) {
        for (y, from, to) in self.rows() {
            for x in from..=to {
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.modifier.insert(Modifier::REVERSED);
                }
            }
        }
    }

    /// What it covers, without what the layout added: trailing spaces,
    /// blank rows at the ends, the bar beside messages, and shared indent.
    pub fn text(&self, buffer: &Buffer) -> String {
        let mut rows: Vec<String> = self
            .rows()
            .map(|(y, from, to)| {
                let mut row = String::new();
                let mut x = from;
                while x <= to {
                    let Some(cell) = buffer.cell((x, y)) else { break };
                    row.push_str(cell.symbol());
                    // A wide character's second column holds a blank.
                    x += cell.symbol().width().max(1) as u16;
                }
                row.truncate(row.trim_end().len());
                row
            })
            .collect();
        for row in &mut rows {
            // The bar down a message's side is decoration, not its text.
            if let Some(at) = row.find(GUTTER)
                && row[..at].chars().all(|c| c == ' ')
            {
                row.replace_range(..at + GUTTER.len(), "");
            }
        }
        // Plain spaces only: counted in bytes, so no cut can split a character.
        let indent = |row: &String| row.len() - row.trim_start_matches(' ').len();
        let shared = rows.iter().skip(1).filter(|r| !r.is_empty()).map(indent).min().unwrap_or(0);
        for row in rows.iter_mut().skip(1).filter(|r| !r.is_empty()) {
            row.drain(..shared);
        }
        if let Some(first) = rows.first_mut() {
            first.drain(..indent(first));
        }
        rows.join("\n").trim_matches('\n').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(rows: &[&str]) -> Buffer {
        Buffer::with_lines(rows.iter().map(|r| r.to_string()))
    }

    fn select(rows: &[&str], from: (u16, u16), to: (u16, u16)) -> (Selection, Buffer) {
        let buffer = buffer(rows);
        let mut selection = Selection::new(from.into(), buffer.area);
        selection.drag_to(to.into());
        (selection, buffer)
    }

    #[test]
    fn takes_part_of_a_row() {
        let (s, b) = select(&["  hello world  "], (2, 0), (6, 0));
        assert_eq!(s.text(&b), "hello");
    }

    #[test]
    fn dragging_backwards_selects_the_same_text() {
        let (s, b) = select(&["  hello world  "], (6, 0), (2, 0));
        assert_eq!(s.text(&b), "hello");
    }

    #[test]
    fn rows_between_the_ends_are_whole_and_lose_their_shared_indent() {
        let rows = ["  one two    ", "  three      ", "    four     ", "  five six   "];
        let (s, b) = select(&rows, (6, 0), (5, 3));
        assert_eq!(s.text(&b), "two\nthree\n  four\nfive");
    }

    #[test]
    fn a_wide_character_is_not_followed_by_a_gap() {
        let (s, b) = select(&["日本語 ok"], (0, 0), (8, 0));
        assert_eq!(s.text(&b), "日本語 ok");
    }

    #[test]
    fn blank_rows_at_the_ends_are_dropped() {
        let (s, b) = select(&["     ", "  hi ", "     "], (0, 0), (4, 2));
        assert_eq!(s.text(&b), "hi");
    }

    #[test]
    fn a_wide_space_in_the_indent_is_kept_whole() {
        let (s, b) = select(&["x", "  a", "\u{3000}b"], (0, 0), (4, 2));
        assert_eq!(s.text(&b), "x\n  a\n\u{3000}b");
    }

    #[test]
    fn the_bar_beside_a_message_is_left_out() {
        let rows = ["  ▎ one two   ", "  ▎ three     ", "  ▎   four    "];
        let (s, b) = select(&rows, (0, 0), (13, 2));
        assert_eq!(s.text(&b), "one two\nthree\n  four");
    }

    #[test]
    fn a_drag_stops_at_the_edge_of_its_pane() {
        let buffer = buffer(&["aaaa", "bbbb", "cccc"]);
        let pane = Rect::new(1, 1, 2, 1);
        let mut selection = Selection::new(Position::new(1, 1), pane);
        selection.drag_to(Position::new(9, 9));
        assert_eq!(selection.text(&buffer), "bb");
    }

    #[test]
    fn paints_only_what_it_covers() {
        let (s, mut b) = select(&["abcd"], (1, 0), (2, 0));
        s.paint(&mut b);
        let reversed = |x: u16| b[(x, 0)].modifier.contains(Modifier::REVERSED);
        assert_eq!((reversed(0), reversed(1), reversed(2), reversed(3)), (false, true, true, false));
    }
}
