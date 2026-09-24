// print.rs
//! `print!` / `println!` over UART0, mirrored to the screen console.
//!
//! Output is plain console text on the Arduino link, so it is passed straight
//! through to the host. Lines must not start with `$` or they are parsed as frames.

use core::fmt::{self, Write};
use crate::drivers::uart::UartWriter;
use crate::synchronization::TryLock;

/// Held while writing a line or a frame, so output from different tasks doesn't interleave on
/// the link (which would break frames).
static OUTPUT: TryLock<()> = TryLock::new(());

/// Runs `write` holding the output lock. If it is already held, this is a panic or exception
/// handler that interrupted a write, and the output goes out anyway rather than being lost.
pub fn serialized<R>(write: impl FnOnce() -> R) -> R {
    let mut write = Some(write);
    OUTPUT
        .try_lock(|_| (write.take().unwrap())())
        .unwrap_or_else(|| (write.take().unwrap())())
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    serialized(|| {
        // UartWriter never returns an error.
        let _ = UartWriter.write_fmt(args);
        super::console::write_fmt(args);
        super::net::mirror(args);
    });
}

/// Prints to UART0.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::sys::print::_print(format_args!($($arg)*)));
}

/// Prints to UART0, with a trailing newline.
#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::sys::print::_print(format_args!("{}\n", format_args!($($arg)*))));
}
