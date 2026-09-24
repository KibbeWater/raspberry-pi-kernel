// line.rs
//! Turning typed characters into lines: what a local keyboard needs before the shell can take
//! its input, since the shell works a line at a time.

use alloc::string::String;

/// A line being typed. Characters are echoed as they come, and backspace takes the last one
/// back, on screen too.
pub struct LineEditor {
    line: String,
    /// Longest line it takes: more characters are dropped.
    max: usize,
}

impl LineEditor {
    pub const fn new(max: usize) -> Self {
        LineEditor { line: String::new(), max }
    }

    /// Takes a typed character, echoing what the screen should show with `echo`. Returns the
    /// finished line, without its newline, once one is typed.
    pub fn feed(&mut self, c: char, mut echo: impl FnMut(&str)) -> Option<String> {
        match c {
            '\n' => {
                echo("\n");
                return Some(core::mem::take(&mut self.line));
            }
            '\x08' => {
                if self.line.pop().is_some() {
                    // Back over it, blank it, and back again.
                    echo("\x08 \x08");
                }
            }
            c if c.is_control() => {}
            c if self.line.len() + c.len_utf8() <= self.max => {
                self.line.push(c);
                let mut bytes = [0; 4];
                echo(c.encode_utf8(&mut bytes));
            }
            _ => {}
        }
        None
    }

    /// Throws the line away, taking it back off the screen too.
    pub fn clear(&mut self, mut echo: impl FnMut(&str)) {
        while self.line.pop().is_some() {
            echo("\x08 \x08");
        }
    }

    /// The line so far, to show again (after the screen was cleared, say).
    pub fn line(&self) -> &str {
        &self.line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_in(editor: &mut LineEditor, text: &str) -> (String, Option<String>) {
        let mut shown = String::new();
        let mut line = None;
        for c in text.chars() {
            if let Some(done) = editor.feed(c, |s| shown.push_str(s)) {
                line = Some(done);
            }
        }
        (shown, line)
    }

    #[test]
    fn a_line_comes_out_when_enter_is_pressed() {
        let mut editor = LineEditor::new(80);
        assert_eq!(type_in(&mut editor, "ls /bin"), ("ls /bin".into(), None));
        assert_eq!(type_in(&mut editor, "\n"), ("\n".into(), Some("ls /bin".into())));
        // The next line starts empty.
        assert_eq!(type_in(&mut editor, "pwd\n").1, Some("pwd".into()));
    }

    #[test]
    fn backspace_takes_back_a_character_on_screen_too() {
        let mut editor = LineEditor::new(80);
        let (shown, line) = type_in(&mut editor, "lx\x08s\n");
        assert_eq!(shown, "lx\x08 \x08s\n");
        assert_eq!(line, Some("ls".into()));
        // Nothing to take back: nothing happens.
        assert_eq!(type_in(&mut editor, "\x08"), (String::new(), None));
    }

    #[test]
    fn clearing_takes_the_whole_line_back() {
        let mut editor = LineEditor::new(80);
        type_in(&mut editor, "oops");
        let mut shown = String::new();
        editor.clear(|s| shown.push_str(s));
        assert_eq!(shown, "\x08 \x08".repeat(4));
        assert_eq!(editor.line(), "");
        assert_eq!(type_in(&mut editor, "ok\n").1, Some("ok".into()));
    }

    #[test]
    fn long_lines_are_cut_and_control_characters_dropped() {
        let mut editor = LineEditor::new(3);
        let (shown, line) = type_in(&mut editor, "ab\tcdef\n");
        assert_eq!(shown, "abc\n");
        assert_eq!(line, Some("abc".into()));
    }
}
