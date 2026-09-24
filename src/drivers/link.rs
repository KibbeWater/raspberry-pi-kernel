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
//! straight through to the host, so `Uart::send_string` logging keeps working.

use crate::drivers::uart::Uart;

/// Link speed. The Uno receives with SoftwareSerial, which is unreliable above this.
pub const BAUD: u32 = 38_400;

/// Longest line (excluding the `\n`) we accept. Keep in sync with the Arduino sketch.
const MAX_LINE: usize = 96;

/// Line assembler for incoming UART bytes.
pub struct Link {
    buf: [u8; MAX_LINE],
    len: usize,
    overflow: bool,
}

impl Link {
    pub const fn new() -> Self {
        Link { buf: [0; MAX_LINE], len: 0, overflow: false }
    }

    /// Drains the UART receive FIFO and calls `on_frame(kind, payload)` for every
    /// complete, checksum-valid frame. Never blocks.
    ///
    /// Malformed frames are answered with an `ERR` frame; plain text lines are dropped.
    pub fn poll(&mut self, mut on_frame: impl FnMut(&str, &str)) {
        while let Some(byte) = Uart::receive() {
            match byte {
                b'\r' => {}
                b'\n' => {
                    if self.overflow {
                        send("ERR", "line too long");
                    } else {
                        dispatch(&self.buf[..self.len], &mut on_frame);
                    }
                    self.len = 0;
                    self.overflow = false;
                }
                _ if self.len < MAX_LINE => {
                    self.buf[self.len] = byte;
                    self.len += 1;
                }
                _ => self.overflow = true,
            }
        }
    }
}

fn dispatch(line: &[u8], on_frame: &mut impl FnMut(&str, &str)) {
    if line.first() != Some(&b'$') {
        return;
    }
    match parse(line) {
        Some((kind, payload)) => on_frame(kind, payload),
        None => send("ERR", "bad frame"),
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

/// Sends one frame. The payload is sent as-is, so it must not contain `\n`.
pub fn send(kind: &str, payload: &str) {
    send_parts(kind, &[payload]);
}

/// Sends one frame whose payload is the concatenation of `parts`, so callers can
/// build messages without an allocator.
pub fn send_parts(kind: &str, parts: &[&str]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut sum = checksum(kind.as_bytes());
    Uart::send(b'$');
    Uart::send_string(kind);
    if parts.iter().any(|p| !p.is_empty()) {
        sum ^= b',';
        Uart::send(b',');
        for part in parts {
            sum ^= checksum(part.as_bytes());
            Uart::send_string(part);
        }
    }
    Uart::send(b'*');
    Uart::send(HEX[(sum >> 4) as usize]);
    Uart::send(HEX[(sum & 0xF) as usize]);
    Uart::send(b'\n');
}

/// Formats `n` as decimal into `buf`, returning the used slice.
pub fn fmt_u32(mut n: u32, buf: &mut [u8; 10]) -> &str {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    // Only ASCII digits were written.
    core::str::from_utf8(&buf[i..]).unwrap_or("?")
}
