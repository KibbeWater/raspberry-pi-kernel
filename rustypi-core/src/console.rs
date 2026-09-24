// console.rs
//! A text console: a grid of characters with a cursor, line wrapping and scrolling. Drawing
//! is left to a `Surface`, so the same logic serves the framebuffer and the tests.

use alloc::vec;
use alloc::vec::Vec;

const TAB_WIDTH: usize = 8;

/// Something that can show one character cell.
pub trait Surface {
    fn draw_cell(&mut self, col: usize, row: usize, c: char);
}

pub struct Console {
    cols: usize,
    rows: usize,
    cells: Vec<char>,
    col: usize,
    row: usize,
}

impl Console {
    /// An empty console of `cols` by `rows` cells (at least one of each).
    pub fn new(cols: usize, rows: usize) -> Self {
        let (cols, rows) = (cols.max(1), rows.max(1));
        Console { cols, rows, cells: vec![' '; cols * rows], col: 0, row: 0 }
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// The character in a cell.
    pub fn cell(&self, col: usize, row: usize) -> char {
        self.cells[row * self.cols + col]
    }

    /// Writes `s`, drawing every cell that changes. Handles `\n`, `\r`, `\t` and backspace
    /// (`\x08`, which moves back a cell without erasing it, like a terminal); other control
    /// characters are ignored.
    pub fn write_str(&mut self, s: &str, surface: &mut impl Surface) {
        for c in s.chars() {
            match c {
                '\n' => self.newline(surface),
                '\r' => self.col = 0,
                '\x08' => self.col = self.col.saturating_sub(1),
                // Like a terminal, a tab moves the cursor without erasing what it passes.
                '\t' => self.col = ((self.col / TAB_WIDTH + 1) * TAB_WIDTH).min(self.cols),
                c if c.is_control() => {}
                c => self.put(c, surface),
            }
        }
    }

    /// Draws every cell again, e.g. after something else drew over the surface.
    pub fn redraw(&self, surface: &mut impl Surface) {
        for row in 0..self.rows {
            for col in 0..self.cols {
                surface.draw_cell(col, row, self.cell(col, row));
            }
        }
    }

    /// Empties the console and moves the cursor to the top left.
    pub fn clear(&mut self, surface: &mut impl Surface) {
        self.cells.fill(' ');
        self.col = 0;
        self.row = 0;
        self.redraw(surface);
    }

    fn put(&mut self, c: char, surface: &mut impl Surface) {
        if self.col == self.cols {
            self.newline(surface);
        }
        self.cells[self.row * self.cols + self.col] = c;
        surface.draw_cell(self.col, self.row, c);
        self.col += 1;
    }

    fn newline(&mut self, surface: &mut impl Surface) {
        self.col = 0;
        if self.row + 1 < self.rows {
            self.row += 1;
            return;
        }
        // Scroll: move every row up one and start the last one empty.
        self.cells.copy_within(self.cols.., 0);
        let last = (self.rows - 1) * self.cols;
        self.cells[last..].fill(' ');
        self.redraw(surface);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    /// Records the grid as drawn, and how many cells were drawn.
    struct Screen {
        cols: usize,
        cells: Vec<char>,
        draws: usize,
    }

    impl Screen {
        fn new(console: &Console) -> Self {
            Screen { cols: console.cols(), cells: vec![' '; console.cols() * console.rows()], draws: 0 }
        }

        fn row(&self, row: usize) -> String {
            self.cells[row * self.cols..(row + 1) * self.cols].iter().collect()
        }
    }

    impl Surface for Screen {
        fn draw_cell(&mut self, col: usize, row: usize, c: char) {
            self.cells[row * self.cols + col] = c;
            self.draws += 1;
        }
    }

    #[test]
    fn writes_and_breaks_lines() {
        let mut console = Console::new(5, 3);
        let mut screen = Screen::new(&console);
        console.write_str("ab\ncd", &mut screen);
        assert_eq!([screen.row(0), screen.row(1)], ["ab   ", "cd   "]);
        // Only the four characters were drawn.
        assert_eq!(screen.draws, 4);
    }

    #[test]
    fn wraps_long_lines() {
        let mut console = Console::new(4, 3);
        let mut screen = Screen::new(&console);
        console.write_str("abcdef", &mut screen);
        assert_eq!([screen.row(0), screen.row(1)], ["abcd", "ef  "]);
    }

    #[test]
    fn exactly_full_lines_do_not_leave_a_blank_line() {
        let mut console = Console::new(4, 3);
        let mut screen = Screen::new(&console);
        console.write_str("abcd\nef", &mut screen);
        assert_eq!([screen.row(0), screen.row(1)], ["abcd", "ef  "]);
    }

    #[test]
    fn scrolls_when_full_and_redraws_what_moved() {
        let mut console = Console::new(3, 2);
        let mut screen = Screen::new(&console);
        console.write_str("one\ntwo\nsix", &mut screen);
        assert_eq!([screen.row(0), screen.row(1)], ["two", "six"]);
        assert_eq!([console.cell(0, 0), console.cell(0, 1)], ['t', 's']);
    }

    #[test]
    fn handles_carriage_returns_tabs_and_control_characters() {
        let mut console = Console::new(12, 2);
        let mut screen = Screen::new(&console);
        console.write_str("abc\rX\x07\ta", &mut screen);
        assert_eq!(screen.row(0), "Xbc     a   ");
    }

    #[test]
    fn backspace_moves_back_so_a_space_erases() {
        let mut console = Console::new(5, 2);
        let mut screen = Screen::new(&console);
        console.write_str("abc\x08 \x08d", &mut screen);
        assert_eq!(screen.row(0), "abd  ");
        // Not past the start of the line.
        console.write_str("\r\x08\x08X", &mut screen);
        assert_eq!(screen.row(0), "Xbd  ");
    }

    #[test]
    fn clear_empties_everything() {
        let mut console = Console::new(3, 2);
        let mut screen = Screen::new(&console);
        console.write_str("abc\ndef", &mut screen);
        console.clear(&mut screen);
        console.write_str("z", &mut screen);
        assert_eq!([screen.row(0), screen.row(1)], ["z  ", "   "]);
    }

    #[test]
    fn a_zero_sized_console_still_works() {
        let mut console = Console::new(0, 0);
        let mut screen = Screen::new(&console);
        console.write_str("hi\n", &mut screen);
        assert_eq!(console.cell(0, 0), ' ');
    }
}
