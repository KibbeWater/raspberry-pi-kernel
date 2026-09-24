// link.rs
//! Line-framed message protocol spoken with the Arduino bridge.
//!
//! Every message is one line terminated by `\n` (`\r` is ignored):
//!
//! ```text
//! $<KIND>[,<payload>]*<HH>\n
//! ```
//!
//! `HH` is the XOR of every byte between `$` and `*`, as two uppercase hex digits.
//! Lines that do not start with `$` are plain console text; the Arduino passes them
//! straight through to the host, so `println!` logging keeps working.
//!
//! A `$` always starts a new frame, so line noise before a frame can't swallow it.
//! Payloads therefore must not contain `$`.

use core::fmt::{self, Write};

/// Longest line (excluding the `\n`) we accept. Keep in sync with the Arduino sketch.
pub const MAX_LINE: usize = 96;

/// Receive-side counters, reported to the Arduino in `STAT` frames.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Bytes received, damaged or not.
    pub rx_bytes: u32,
    /// Bytes the UART flagged with a framing, parity, break or overrun error.
    pub rx_errors: u32,
    /// Partial or non-frame lines thrown away.
    pub dropped: u32,
    /// Frames with a bad checksum or layout.
    pub bad_frames: u32,
    /// Lines longer than `MAX_LINE`.
    pub overflows: u32,
}

/// What a completed line turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// A checksum-valid frame.
    Frame { kind: &'a str, payload: &'a str },
    /// A line that should be answered with an `ERR` frame carrying this reason.
    Rejected(&'static str),
}

/// Assembles received bytes into frames.
pub struct Receiver {
    buf: [u8; MAX_LINE],
    len: usize,
    overflow: bool,
    pub stats: Stats,
}

impl Receiver {
    pub const fn new() -> Self {
        Receiver {
            buf: [0; MAX_LINE],
            len: 0,
            overflow: false,
            stats: Stats { rx_bytes: 0, rx_errors: 0, dropped: 0, bad_frames: 0, overflows: 0 },
        }
    }

    /// Feeds one received byte, `Err` if the UART flagged it as damaged. Calls `on_event`
    /// when it completes a frame or a line that must be rejected; plain text lines are
    /// dropped.
    pub fn push(&mut self, received: Result<u8, u8>, mut on_event: impl FnMut(Event)) {
        self.stats.rx_bytes = self.stats.rx_bytes.wrapping_add(1);
        let byte = match received {
            Ok(byte) => byte,
            Err(_) => {
                // The line this byte belonged to is damaged; discard it.
                self.stats.rx_errors = self.stats.rx_errors.wrapping_add(1);
                self.discard();
                return;
            }
        };
        match byte {
            b'\r' => {}
            b'\n' => {
                if self.overflow {
                    self.stats.overflows = self.stats.overflows.wrapping_add(1);
                    on_event(Event::Rejected("line too long"));
                } else if self.len > 0 {
                    self.dispatch(&mut on_event);
                }
                self.len = 0;
                self.overflow = false;
            }
            b'$' => {
                self.discard();
                self.store(byte);
            }
            _ => self.store(byte),
        }
    }

    fn store(&mut self, byte: u8) {
        if self.len < MAX_LINE {
            self.buf[self.len] = byte;
            self.len += 1;
        } else {
            self.overflow = true;
        }
    }

    /// Throws away the partial line, counting it if there was one.
    fn discard(&mut self) {
        if self.len > 0 || self.overflow {
            self.stats.dropped = self.stats.dropped.wrapping_add(1);
        }
        self.len = 0;
        self.overflow = false;
    }

    fn dispatch(&mut self, on_event: &mut impl FnMut(Event)) {
        let line = &self.buf[..self.len];
        if line[0] != b'$' {
            self.stats.dropped = self.stats.dropped.wrapping_add(1);
            return;
        }
        match parse(line) {
            Some((kind, payload)) => on_event(Event::Frame { kind, payload }),
            None => {
                self.stats.bad_frames = self.stats.bad_frames.wrapping_add(1);
                on_event(Event::Rejected("bad frame"));
            }
        }
    }
}

/// Splits `$KIND,payload*HH` into `(KIND, payload)` after verifying the checksum.
pub fn parse(line: &[u8]) -> Option<(&str, &str)> {
    if line.len() < 4 || line[0] != b'$' || line[line.len() - 3] != b'*' {
        return None;
    }
    let body = &line[1..line.len() - 3];
    let expected = hex_byte(line[line.len() - 2], line[line.len() - 1])?;
    if checksum(body) != expected {
        return None;
    }
    let body = core::str::from_utf8(body).ok()?;
    Some(body.split_once(',').unwrap_or((body, "")))
}

fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |acc, b| acc ^ b)
}

fn hex_byte(hi: u8, lo: u8) -> Option<u8> {
    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'A'..=b'F' => Some(c - b'A' + 10),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        }
    }
    Some(nibble(hi)? << 4 | nibble(lo)?)
}

/// Writes one frame with a formatted payload to `out`, without needing an allocator. A `$`,
/// `\r` or `\n` in the payload would break the framing, so each is sent as `?` instead.
pub fn write_frame(out: &mut impl Write, kind: &str, payload: fmt::Arguments) -> fmt::Result {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    out.write_char('$')?;
    out.write_str(kind)?;
    let mut frame = FrameWriter { out, sum: checksum(kind.as_bytes()), has_payload: false };
    frame.write_fmt(payload)?;
    let sum = frame.sum;
    out.write_char('*')?;
    out.write_char(HEX[(sum >> 4) as usize] as char)?;
    out.write_char(HEX[(sum & 0xF) as usize] as char)?;
    out.write_char('\n')
}

/// Streams a payload while accumulating the frame checksum. The `,` separator is only
/// written once the payload turns out to be non-empty.
struct FrameWriter<'a, W: Write> {
    out: &'a mut W,
    sum: u8,
    has_payload: bool,
}

impl<W: Write> Write for FrameWriter<'_, W> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if s.is_empty() {
            return Ok(());
        }
        if !self.has_payload {
            self.has_payload = true;
            self.sum ^= b',';
            self.out.write_char(',')?;
        }
        for piece in s.split_inclusive(FRAME_BREAKING) {
            let (text, replaced) = match piece.strip_suffix(FRAME_BREAKING) {
                Some(text) => (text, true),
                None => (piece, false),
            };
            self.sum ^= checksum(text.as_bytes());
            self.out.write_str(text)?;
            if replaced {
                self.sum ^= b'?';
                self.out.write_char('?')?;
            }
        }
        Ok(())
    }
}

/// Characters that can't appear inside a frame.
const FRAME_BREAKING: [char; 3] = ['$', '\r', '\n'];

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn frame(kind: &str, payload: &str) -> String {
        let mut out = String::new();
        write_frame(&mut out, kind, format_args!("{}", payload)).unwrap();
        out
    }

    /// Feeds `input` and returns the events as owned strings.
    fn feed(receiver: &mut Receiver, input: &[u8]) -> Vec<String> {
        let mut events = Vec::new();
        for &byte in input {
            receiver.push(Ok(byte), |event| {
                events.push(match event {
                    Event::Frame { kind, payload } => format!("{}|{}", kind, payload),
                    Event::Rejected(reason) => format!("ERR {}", reason),
                })
            });
        }
        events
    }

    #[test]
    fn writes_frames_matching_the_arduino() {
        // Same vectors as the Arduino's test/protocol_test.cpp.
        assert_eq!(frame("PING", "1"), "$PING,1*0D\n");
        assert_eq!(frame("PONG", ""), "$PONG*16\n");
    }

    #[test]
    fn empty_formatted_parts_do_not_add_a_separator() {
        let mut out = String::new();
        write_frame(&mut out, "PONG", format_args!("{}{}", "", "")).unwrap();
        assert_eq!(out, "$PONG*16\n");
    }

    #[test]
    fn frame_breaking_characters_are_replaced() {
        let line = frame("RSP", "cost $5\r\nnext");
        assert_eq!(line, frame("RSP", "cost ?5??next"));
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn parses_what_it_writes() {
        let line = frame("RSP", "3,0,led is on, really");
        let line = line.trim_end().as_bytes();
        assert_eq!(parse(line), Some(("RSP", "3,0,led is on, really")));
    }

    #[test]
    fn parse_rejects_malformed_lines() {
        assert_eq!(parse(b"$PING,1*0d"), Some(("PING", "1")));
        assert_eq!(parse(b"$PING,1*0E"), None);
        assert_eq!(parse(b"$PING,1*ZZ"), None);
        assert_eq!(parse(b"$PING,1"), None);
        assert_eq!(parse(b"PING,1*0D"), None);
        assert_eq!(parse(b"$*0"), None);
    }

    #[test]
    fn receiver_delivers_frames_and_drops_plain_text() {
        let mut receiver = Receiver::new();
        let mut input = Vec::from(b"hello there\r\n" as &[u8]);
        input.extend_from_slice(frame("MSG", "1,help").as_bytes());
        assert_eq!(feed(&mut receiver, &input), ["MSG|1,help"]);
        assert_eq!(receiver.stats.dropped, 1);
        assert_eq!(receiver.stats.rx_bytes, input.len() as u32);
    }

    #[test]
    fn receiver_resyncs_on_dollar() {
        // A frame that lost its end is cut off by the next one.
        let mut receiver = Receiver::new();
        let mut input = Vec::from(b"$PONG,jf)" as &[u8]);
        input.extend_from_slice(frame("PONG", "16").as_bytes());
        assert_eq!(feed(&mut receiver, &input), ["PONG|16"]);
        assert_eq!(receiver.stats.dropped, 1);
    }

    #[test]
    fn receiver_rejects_bad_checksums_and_long_lines() {
        let mut receiver = Receiver::new();
        assert_eq!(feed(&mut receiver, b"$PING,1*0E\n"), ["ERR bad frame"]);
        assert_eq!(receiver.stats.bad_frames, 1);

        let mut long = Vec::from([b'x'; MAX_LINE + 5]);
        long.push(b'\n');
        assert_eq!(feed(&mut receiver, &long), ["ERR line too long"]);
        assert_eq!(receiver.stats.overflows, 1);

        // It recovers for the next frame.
        assert_eq!(feed(&mut receiver, frame("PING", "2").as_bytes()), ["PING|2"]);
    }

    #[test]
    fn receiver_discards_lines_with_damaged_bytes() {
        let mut receiver = Receiver::new();
        let good = frame("PING", "3");
        let mut events = feed(&mut receiver, &good.as_bytes()[..4]);
        receiver.push(Err(0), |_| panic!("no event expected"));
        events.extend(feed(&mut receiver, &good.as_bytes()[4..]));
        assert!(events.is_empty());
        assert_eq!(receiver.stats.rx_errors, 1);
        assert_eq!(receiver.stats.dropped, 2); // the damaged start, then its orphaned rest
    }
}
