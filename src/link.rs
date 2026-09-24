// link.rs
//! Line-framed message protocol spoken with the Arduino bridge over UART0.
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
use crate::drivers::uart::Uart;

/// Link speed. The Uno receives with SoftwareSerial, which is unreliable above this.
pub const BAUD: u32 = 38_400;

/// Longest line (excluding the `\n`) we accept. Keep in sync with the Arduino sketch.
const MAX_LINE: usize = 96;

/// Receive-side counters, reported to the Arduino in `STAT` frames.
pub struct Stats {
    /// Bytes read from the UART, damaged or not.
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

/// Line assembler for incoming UART bytes.
pub struct Link {
    buf: [u8; MAX_LINE],
    len: usize,
    overflow: bool,
    pub stats: Stats,
}

impl Link {
    pub const fn new() -> Self {
        Link {
            buf: [0; MAX_LINE],
            len: 0,
            overflow: false,
            stats: Stats { rx_bytes: 0, rx_errors: 0, dropped: 0, bad_frames: 0, overflows: 0 },
        }
    }

    /// Drains the UART receive FIFO and calls `on_frame(kind, payload)` for every
    /// complete, checksum-valid frame. Never blocks.
    ///
    /// Malformed frames are answered with an `ERR` frame; plain text lines are dropped.
    pub fn poll(&mut self, mut on_frame: impl FnMut(&str, &str)) {
        while let Some(received) = Uart::receive_checked() {
            self.stats.rx_bytes = self.stats.rx_bytes.wrapping_add(1);
            let byte = match received {
                Ok(byte) => byte,
                Err(_) => {
                    // The line this byte belonged to is damaged; discard it.
                    self.stats.rx_errors = self.stats.rx_errors.wrapping_add(1);
                    self.discard();
                    continue;
                }
            };
            match byte {
                b'\r' => {}
                b'\n' => {
                    if self.overflow {
                        self.stats.overflows = self.stats.overflows.wrapping_add(1);
                        send("ERR", "line too long");
                    } else if self.len > 0 {
                        self.dispatch(&mut on_frame);
                    }
                    self.len = 0;
                    self.overflow = false;
                }
                b'$' => {
                    self.discard();
                    self.push(byte);
                }
                _ => self.push(byte),
            }
        }
    }

    fn push(&mut self, byte: u8) {
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

    fn dispatch(&mut self, on_frame: &mut impl FnMut(&str, &str)) {
        let line = &self.buf[..self.len];
        if line[0] != b'$' {
            self.stats.dropped = self.stats.dropped.wrapping_add(1);
            return;
        }
        match parse(line) {
            Some((kind, payload)) => on_frame(kind, payload),
            None => {
                self.stats.bad_frames = self.stats.bad_frames.wrapping_add(1);
                send("ERR", "bad frame");
            }
        }
    }
}

/// Splits `$KIND,payload*HH` into `(KIND, payload)` after verifying the checksum.
fn parse(line: &[u8]) -> Option<(&str, &str)> {
    if line.len() < 4 || line[line.len() - 3] != b'*' {
        return None;
    }
    let body = &line[1..line.len() - 3];
    let expected = hex_byte(line[line.len() - 2], line[line.len() - 1])?;
    if checksum(body) != expected {
        return None;
    }
    let body = core::str::from_utf8(body).ok()?;
    Some(match body.find(',') {
        Some(i) => (&body[..i], &body[i + 1..]),
        None => (body, ""),
    })
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

/// Sends one frame. The payload must not contain `$` or `\n`.
pub fn send(kind: &str, payload: &str) {
    send_fmt(kind, format_args!("{}", payload));
}

/// Sends one frame with a formatted payload, without needing an allocator:
///
/// ```ignore
/// link::send_fmt("RSP", format_args!("uptime {}s", secs));
/// ```
pub fn send_fmt(kind: &str, payload: fmt::Arguments) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    Uart::send(b'$');
    Uart::send_string(kind);
    let mut frame = FrameWriter { sum: checksum(kind.as_bytes()), has_payload: false };
    // FrameWriter never returns an error.
    let _ = frame.write_fmt(payload);
    let sum = frame.sum;
    Uart::send(b'*');
    Uart::send(HEX[(sum >> 4) as usize]);
    Uart::send(HEX[(sum & 0xF) as usize]);
    Uart::send(b'\n');
}

/// Streams a payload to the UART while accumulating the frame checksum. The `,`
/// separator is only sent once the payload turns out to be non-empty.
struct FrameWriter {
    sum: u8,
    has_payload: bool,
}

impl Write for FrameWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if s.is_empty() {
            return Ok(());
        }
        if !self.has_payload {
            self.has_payload = true;
            self.sum ^= b',';
            Uart::send(b',');
        }
        self.sum ^= checksum(s.as_bytes());
        Uart::send_string(s);
        Ok(())
    }
}
