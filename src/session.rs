// session.rs
//! Reliable request/response on top of `link` frames.
//!
//! The Uno sends each command as `MSG,<seq>,<text>` and retransmits it until it has the
//! complete reply: zero or more numbered lines `RSP|HELP,<seq>,<index>,<text>`, then
//! `END,<seq>,<count>`. A request with the same `seq` as the last one handled is a
//! retransmit, so its reply is resent from the cache instead of running the command
//! twice (a retransmitted `led toggle` must not toggle again).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use crate::link;

/// Longest line text that still fits in a frame: `$HELP,65535,99,<text>*HH` must stay
/// within the 96-byte line limit.
const MAX_TEXT: usize = 78;

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
        self.line(LineKind::Rsp, format_args!("{}", text));
    }
}

pub struct Session {
    last_seq: Option<u16>,
    reply: Reply,
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
    /// performs after the reply has been sent.
    pub fn handle<'a, A>(
        &mut self,
        payload: &'a str,
        run: impl FnOnce(&'a str, &mut Reply) -> Option<A>,
    ) -> Option<A> {
        let Some((seq, text)) = parse_request(payload) else {
            link::send("ERR", "MSG needs a sequence number");
            return None;
        };
        if self.last_seq == Some(seq) {
            self.send_reply(seq);
            return None;
        }
        self.reply.lines.clear();
        let action = run(text, &mut self.reply);
        self.last_seq = Some(seq);
        self.send_reply(seq);
        action
    }

    fn send_reply(&self, seq: u16) {
        for (index, (kind, text)) in self.reply.lines.iter().enumerate() {
            link::send_fmt(kind.frame_kind(), format_args!("{},{},{}", seq, index, text));
        }
        link::send_fmt("END", format_args!("{},{}", seq, self.reply.lines.len()));
    }
}

/// Splits `<seq>,<text>`.
fn parse_request(payload: &str) -> Option<(u16, &str)> {
    let (seq, text) = payload.split_once(',').unwrap_or((payload, ""));
    Some((seq.parse().ok()?, text))
}
