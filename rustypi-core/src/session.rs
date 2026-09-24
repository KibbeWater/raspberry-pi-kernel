// session.rs
//! Reliable request/response on top of `link` frames.
//!
//! The Uno sends each command as `MSG,<seq>,<text>` and retransmits it until it has the
//! complete reply: zero or more numbered lines `RSP|HELP,<seq>,<index>,<text>`, then
//! `END,<seq>,<count>`. A request with the same `seq` as the last one handled is a
//! retransmit, so its reply is resent from the cache instead of running the command
//! twice (a retransmitted `led toggle` must not toggle again).
//!
//! A retransmit that arrives while its command is still running is answered with
//! `BUSY,<seq>` (by the kernel's link task, which reads frames while the shell works), so
//! the Uno keeps waiting instead of giving up on a slow command.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};

/// Longest line text that still fits in a frame: `$HELP,65535,99,<text>*HH` must stay
/// within the link's line limit.
const MAX_TEXT: usize = crate::link::MAX_LINE - "$HELP,65535,99,*HH".len();

#[derive(Clone, Copy)]
pub enum LineKind {
    /// Reply text.
    Rsp,
    /// One entry of the command list.
    Help,
}

impl LineKind {
    fn frame_kind(self) -> &'static str {
        match self {
            LineKind::Rsp => "RSP",
            LineKind::Help => "HELP",
        }
    }
}

/// The lines a command replies with, kept so they can be resent. Text must not contain
/// `$` or `\n`; lines longer than `MAX_TEXT` are cut short.
pub struct Reply {
    lines: Vec<(LineKind, String)>,
}

impl Reply {
    const fn new() -> Self {
        Reply { lines: Vec::new() }
    }

    /// Adds a formatted line.
    pub fn line(&mut self, kind: LineKind, text: fmt::Arguments) {
        let mut line = String::new();
        // Writing to a String only fails if a Display impl does.
        let _ = line.write_fmt(text);
        if line.len() > MAX_TEXT {
            let mut end = MAX_TEXT;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            line.truncate(end);
        }
        self.lines.push((kind, line));
    }

    /// Adds a plain reply line.
    pub fn rsp(&mut self, text: &str) {
        self.line(LineKind::Rsp, format_args!("{text}"));
    }
}

pub struct Session {
    last_seq: Option<u16>,
    reply: Reply,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub const fn new() -> Self {
        Session { last_seq: None, reply: Reply::new() }
    }

    /// Forgets the last request. Called when the Uno (re)starts, since its sequence
    /// numbers start over.
    pub fn reset(&mut self) {
        self.last_seq = None;
    }

    /// Handles the payload of a `MSG` frame. For a new request, `run` executes the command
    /// and fills in the reply; it may return an action (like a reboot) that the caller
    /// performs after the reply has been sent. Frames are sent with `send(kind, payload)`.
    pub fn handle<'a, A>(
        &mut self,
        payload: &'a str,
        run: impl FnOnce(&'a str, &mut Reply) -> Option<A>,
        mut send: impl FnMut(&str, fmt::Arguments),
    ) -> Option<A> {
        let Some((seq, text)) = parse_request(payload) else {
            send("ERR", format_args!("MSG needs a sequence number"));
            return None;
        };
        if self.last_seq == Some(seq) {
            self.send_reply(seq, &mut send);
            return None;
        }
        self.reply.lines.clear();
        let action = run(text, &mut self.reply);
        self.last_seq = Some(seq);
        self.send_reply(seq, &mut send);
        action
    }

    fn send_reply(&self, seq: u16, send: &mut impl FnMut(&str, fmt::Arguments)) {
        for (index, (kind, text)) in self.reply.lines.iter().enumerate() {
            send(kind.frame_kind(), format_args!("{seq},{index},{text}"));
        }
        send("END", format_args!("{},{}", seq, self.reply.lines.len()));
    }
}

/// The sequence number of a `MSG` payload, without handling it.
pub fn request_seq(payload: &str) -> Option<u16> {
    parse_request(payload).map(|(seq, _)| seq)
}

/// Splits `<seq>,<text>`.
fn parse_request(payload: &str) -> Option<(u16, &str)> {
    let (seq, text) = payload.split_once(',').unwrap_or((payload, ""));
    Some((seq.parse().ok()?, text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;

    /// Runs one `MSG` payload through `session`, returning the frames sent and how many
    /// times the command ran.
    fn exchange(session: &mut Session, payload: &str, runs: &mut u32) -> Vec<String> {
        let mut sent = Vec::new();
        let action = session.handle(
            payload,
            |text, reply| {
                *runs += 1;
                match text {
                    "help" => {
                        reply.line(LineKind::Help, format_args!("a  first"));
                        reply.line(LineKind::Help, format_args!("b  second"));
                    }
                    "quiet" => {}
                    "reboot" => {
                        reply.rsp("rebooting");
                        return Some("reboot");
                    }
                    _ => reply.line(LineKind::Rsp, format_args!("echo: {text}")),
                }
                None
            },
            |kind, payload| sent.push(format!("{kind} {payload}")),
        );
        if let Some(action) = action {
            sent.push(format!("action {action}"));
        }
        sent
    }

    #[test]
    fn numbers_reply_lines_and_ends_with_the_count() {
        let mut session = Session::new();
        let mut runs = 0;
        assert_eq!(
            exchange(&mut session, "7,help", &mut runs),
            ["HELP 7,0,a  first", "HELP 7,1,b  second", "END 7,2"],
        );
        assert_eq!(exchange(&mut session, "8,quiet", &mut runs), ["END 8,0"]);
    }

    #[test]
    fn retransmits_are_answered_from_the_cache_without_rerunning() {
        let mut session = Session::new();
        let mut runs = 0;
        let first = exchange(&mut session, "5,led toggle", &mut runs);
        let again = exchange(&mut session, "5,led toggle", &mut runs);
        assert_eq!(first, again);
        assert_eq!(runs, 1);
        // A new sequence number runs it again.
        exchange(&mut session, "6,led toggle", &mut runs);
        assert_eq!(runs, 2);
    }

    #[test]
    fn reset_forgets_the_last_request() {
        let mut session = Session::new();
        let mut runs = 0;
        exchange(&mut session, "1,x", &mut runs);
        session.reset();
        exchange(&mut session, "1,x", &mut runs);
        assert_eq!(runs, 2);
    }

    #[test]
    fn actions_come_after_the_reply_and_only_once() {
        let mut session = Session::new();
        let mut runs = 0;
        assert_eq!(
            exchange(&mut session, "9,reboot", &mut runs),
            ["RSP 9,0,rebooting", "END 9,1", "action reboot"],
        );
        // A retransmit gets the reply again but not the action.
        assert_eq!(exchange(&mut session, "9,reboot", &mut runs), ["RSP 9,0,rebooting", "END 9,1"]);
    }

    #[test]
    fn requests_without_a_sequence_number_are_rejected() {
        let mut session = Session::new();
        let mut runs = 0;
        for payload in ["help", "", "x,help", "70000,help"] {
            assert_eq!(
                exchange(&mut session, payload, &mut runs),
                ["ERR MSG needs a sequence number"],
            );
        }
        assert_eq!(runs, 0);
        // An empty command is fine.
        assert_eq!(exchange(&mut session, "3", &mut runs), ["RSP 3,0,echo: ", "END 3,1"]);
    }

    #[test]
    fn request_seq_reads_the_sequence_number_only() {
        assert_eq!(request_seq("42,led toggle"), Some(42));
        assert_eq!(request_seq("7"), Some(7));
        assert_eq!(request_seq("x,help"), None);
        assert_eq!(request_seq("70000,help"), None);
    }

    #[test]
    fn the_longest_line_text_fits_the_largest_frame() {
        let mut frame = String::new();
        let text = "x".repeat(MAX_TEXT);
        crate::link::write_frame(&mut frame, "HELP", format_args!("65535,99,{text}")).unwrap();
        assert_eq!(frame.trim_end().len(), crate::link::MAX_LINE);
    }

    #[test]
    fn long_lines_are_cut_at_a_character_boundary() {
        let mut reply = Reply::new();
        let long = "é".repeat(MAX_TEXT); // twice MAX_TEXT bytes
        reply.line(LineKind::Rsp, format_args!("{long}"));
        let text = &reply.lines[0].1;
        assert!(text.len() <= MAX_TEXT);
        assert_eq!(*text, "é".repeat(MAX_TEXT / 2));
        // Short lines are kept whole.
        reply.rsp("short");
        assert_eq!(reply.lines[1].1, "short".to_string());
    }
}
