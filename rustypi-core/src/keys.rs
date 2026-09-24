// keys.rs
//! Keyboard input between keys and characters: accents that combine with the next letter
//! (dead keys), and keys that repeat while held. Keyboards report only which keys are down,
//! so both are the host's job.

/// Puts an accent from a dead key on the next letter.
#[derive(Default)]
pub struct Composer {
    accent: Option<char>,
}

impl Composer {
    pub const fn new() -> Self {
        Composer { accent: None }
    }

    /// A dead key was pressed: its accent goes on the next character. Pressed twice, it types
    /// the accent itself.
    pub fn dead(&mut self, accent: char, mut out: impl FnMut(char)) {
        match self.accent.take() {
            Some(pending) if pending == accent => out(accent),
            Some(pending) => {
                out(pending);
                self.accent = Some(accent);
            }
            None => self.accent = Some(accent),
        }
    }

    /// A character was typed: with an accent waiting, the accented letter if there is one,
    /// else the accent and then the character (a space just gives the accent). Backspace
    /// takes a waiting accent back instead.
    pub fn char(&mut self, c: char, mut out: impl FnMut(char)) {
        let Some(accent) = self.accent.take() else {
            out(c);
            return;
        };
        match (compose(accent, c), c) {
            (Some(accented), _) => out(accented),
            (None, '\x08') => {}
            (None, ' ') => out(accent),
            (None, c) => {
                out(accent);
                out(c);
            }
        }
    }

    /// Forgets a waiting accent.
    pub fn reset(&mut self) {
        self.accent = None;
    }
}

/// `letter` with `accent` on it, if Latin-1 has it.
fn compose(accent: char, letter: char) -> Option<char> {
    let (plain, accented): (&str, &str) = match accent {
        '´' => ("aeiouyAEIOUY", "áéíóúýÁÉÍÓÚÝ"),
        '`' => ("aeiouAEIOU", "àèìòùÀÈÌÒÙ"),
        '¨' => ("aeiouyAEIOU", "äëïöüÿÄËÏÖÜ"),
        '^' => ("aeiouAEIOU", "âêîôûÂÊÎÔÛ"),
        '~' => ("anoANO", "ãñõÃÑÕ"),
        _ => return None,
    };
    let index = plain.chars().position(|c| c == letter)?;
    accented.chars().nth(index)
}

/// Makes a held key type again: after `DELAY_US`, then every `RATE_US`, like a PC's typematic
/// repeat. Only the key pressed last repeats.
pub struct Repeater {
    /// The key, and when it next repeats.
    held: Option<(u8, u64)>,
}

impl Default for Repeater {
    fn default() -> Self {
        Self::new()
    }
}

impl Repeater {
    pub const DELAY_US: u64 = 500_000;
    pub const RATE_US: u64 = 33_000;

    pub const fn new() -> Self {
        Repeater { held: None }
    }

    /// A key that repeats was pressed at `now_us`.
    pub fn pressed(&mut self, key: u8, now_us: u64) {
        self.held = Some((key, now_us + Self::DELAY_US));
    }

    /// Stops repeating, whatever is held.
    pub fn stop(&mut self) {
        self.held = None;
    }

    /// Called often, with the keys held now: the key to type again, if its time has come. A
    /// released key stops repeating.
    pub fn due(&mut self, held: &[u8], now_us: u64) -> Option<u8> {
        let (key, next) = self.held?;
        if !held.contains(&key) {
            self.held = None;
            return None;
        }
        if now_us < next {
            return None;
        }
        // From when it was due rather than now, so a slightly late poll doesn't slow the
        // rate; but a whole repeat or more behind, it starts afresh rather than bursting.
        let following = if next + Self::RATE_US > now_us { next + Self::RATE_US } else { now_us + Self::RATE_US };
        self.held = Some((key, following));
        Some(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn typed(steps: &[Result<char, char>]) -> String {
        let mut composer = Composer::new();
        let mut out = String::new();
        for step in steps {
            match *step {
                Ok(c) => composer.char(c, |c| out.push(c)),
                Err(accent) => composer.dead(accent, |c| out.push(c)),
            }
        }
        out
    }

    #[test]
    fn accents_combine_with_the_next_letter() {
        assert_eq!(typed(&[Err('´'), Ok('e')]), "é");
        assert_eq!(typed(&[Err('¨'), Ok('U'), Err('~'), Ok('n')]), "Üñ");
        assert_eq!(typed(&[Ok('c'), Err('^'), Ok('o'), Ok('t'), Ok('e')]), "côte");
    }

    #[test]
    fn accents_that_dont_combine_are_typed_themselves() {
        // Before a space, twice, or before a letter it doesn't go on.
        assert_eq!(typed(&[Err('´'), Ok(' ')]), "´");
        assert_eq!(typed(&[Err('^'), Err('^')]), "^");
        assert_eq!(typed(&[Err('~'), Ok('x')]), "~x");
        // Another accent: the first is typed, the second waits.
        assert_eq!(typed(&[Err('´'), Err('¨'), Ok('o')]), "´ö");
        // Backspace takes a waiting accent back.
        assert_eq!(typed(&[Err('´'), Ok('\x08'), Ok('e')]), "e");
    }

    #[test]
    fn a_held_key_repeats_after_a_delay() {
        let mut repeater = Repeater::new();
        repeater.pressed(0x04, 0);
        let held = [0x04];
        assert_eq!(repeater.due(&held, Repeater::DELAY_US - 1), None);
        assert_eq!(repeater.due(&held, Repeater::DELAY_US), Some(0x04));
        assert_eq!(repeater.due(&held, Repeater::DELAY_US + 1), None);
        assert_eq!(repeater.due(&held, Repeater::DELAY_US + Repeater::RATE_US), Some(0x04));
        // Released: it stops, and stays stopped.
        assert_eq!(repeater.due(&[], Repeater::DELAY_US + 2 * Repeater::RATE_US), None);
        assert_eq!(repeater.due(&held, Repeater::DELAY_US + 3 * Repeater::RATE_US), None);
    }

    #[test]
    fn only_the_last_key_pressed_repeats() {
        let mut repeater = Repeater::new();
        repeater.pressed(0x04, 0);
        repeater.pressed(0x05, 100_000);
        let held = [0x04, 0x05];
        let at = 100_000 + Repeater::DELAY_US;
        assert_eq!(repeater.due(&held, at), Some(0x05));
        let repeats: Vec<_> = (1..5).filter_map(|i| repeater.due(&held, at + i * Repeater::RATE_US)).collect();
        assert_eq!(repeats, [0x05; 4]);
        // A slow poll catches up by one repeat, not a burst.
        let late = at + 100 * Repeater::RATE_US;
        assert_eq!(repeater.due(&held, late), Some(0x05));
        assert_eq!(repeater.due(&held, late + 1), None);
    }
}
