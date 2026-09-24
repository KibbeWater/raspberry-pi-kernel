// print.rs
//! `print!` / `println!` over UART0, mirrored to the screen console.
//!
//! Output is plain console text on the Arduino link, so it is passed straight
//! through to the host. Lines must not start with `$` or they are parsed as frames.

use core::fmt::{self, Write};
use crate::drivers::uart::UartWriter;

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    // UartWriter never returns an error.
    let _ = UartWriter.write_fmt(args);
    super::console::write_fmt(args);
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
