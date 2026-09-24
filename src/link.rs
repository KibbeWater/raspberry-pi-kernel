// link.rs
//! The Arduino link over UART0: `rustypi_core::link` framing, fed from and written to
//! the UART.

use core::fmt;
use rustypi_core::link::{self as frames, Event, Receiver};
use crate::drivers::uart::{Uart, UartWriter};

pub use rustypi_core::link::Stats;

/// Link speed. Must match LINK_BAUD in the Arduino sketch.
pub const BAUD: u32 = 38_400;

pub struct Link {
    receiver: Receiver,
}

impl Link {
    pub const fn new() -> Self {
        Link { receiver: Receiver::new() }
    }

    pub fn stats(&self) -> &Stats {
        &self.receiver.stats
    }

    /// Drains the UART receive queue and calls `on_frame(kind, payload)` for every
    /// complete, checksum-valid frame. Never blocks.
    ///
    /// Malformed frames are answered with an `ERR` frame; plain text lines are dropped.
    pub fn poll(&mut self, mut on_frame: impl FnMut(&str, &str)) {
        while let Some(received) = Uart::receive_checked() {
            self.receiver.push(received, |event| match event {
                Event::Frame { kind, payload } => on_frame(kind, payload),
                Event::Rejected(reason) => send("ERR", reason),
            });
        }
    }
}

/// Sends one frame. The payload must not contain `$` or `\n`.
pub fn send(kind: &str, payload: &str) {
    send_fmt(kind, format_args!("{payload}"));
}

/// Sends one frame with a formatted payload, without needing an allocator:
///
/// ```ignore
/// link::send_fmt("RSP", format_args!("uptime {}s", secs));
/// ```
pub fn send_fmt(kind: &str, payload: fmt::Arguments) {
    crate::sys::print::serialized(|| {
        // UartWriter never returns an error.
        let _ = frames::write_frame(&mut UartWriter, kind, payload);
    });
}
