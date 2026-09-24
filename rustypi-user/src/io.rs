//! Console output.

use core::fmt::{self, Write};
use rustypi_abi::{Errno, Syscall, MAX_WRITE};
use crate::syscall;

/// Writes bytes to the console, returning how many went (at most `MAX_WRITE`).
pub fn write(bytes: &[u8]) -> Result<usize, Errno> {
    let call = Syscall::Write { ptr: bytes.as_ptr() as u64, len: bytes.len() as u64 };
    syscall::call(call).map(|written| written as usize)
}

/// Writes all of `bytes` to the console.
pub fn write_all(mut bytes: &[u8]) -> Result<(), Errno> {
    while !bytes.is_empty() {
        let written = write(bytes)?;
        bytes = &bytes[written..];
    }
    Ok(())
}

/// Collects formatted output, so a `println!` is usually a single system call.
struct Buffered {
    buf: [u8; MAX_WRITE],
    len: usize,
}

impl Buffered {
    fn flush(&mut self) -> fmt::Result {
        let result = write_all(&self.buf[..self.len]).map_err(|_| fmt::Error);
        self.len = 0;
        result
    }
}

impl Write for Buffered {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut bytes = s.as_bytes();
        while !bytes.is_empty() {
            if self.len == MAX_WRITE {
                self.flush()?;
            }
            let n = bytes.len().min(MAX_WRITE - self.len);
            self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
            self.len += n;
            bytes = &bytes[n..];
        }
        Ok(())
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let mut out = Buffered { buf: [0; MAX_WRITE], len: 0 };
    // There is nowhere to report a failed console write.
    let _ = out.write_fmt(args).and_then(|()| out.flush());
}

/// Prints to the console.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::io::_print(format_args!($($arg)*)));
}

/// Prints to the console, with a trailing newline.
#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::io::_print(format_args!("{}\n", format_args!($($arg)*))));
}
