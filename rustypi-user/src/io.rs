//! Console output, and input typed while the program runs in the foreground.
//!
//! The console shows whole lines, so end prompts with a newline (`println!`).

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use rustypi_abi::{Errno, Syscall, INPUT, MAX_WRITE};
use crate::syscall;

/// Reads input into `buf`, waiting until there is some. Returns how many bytes were read.
/// Input comes a line at a time, each ending with `\n`.
pub fn read(buf: &mut [u8]) -> Result<usize, Errno> {
    let call = Syscall::Read { handle: INPUT, ptr: buf.as_mut_ptr() as u64, len: buf.len() as u64 };
    syscall::call(call).map(|read| read as usize)
}

/// Waits for the next line of input, and returns it without its `\n`.
pub fn read_line() -> Result<String, Errno> {
    let mut line = Vec::new();
    let mut byte = [0];
    // A byte at a time, so nothing past the line is taken from the kernel's queue.
    loop {
        if read(&mut byte)? == 1 {
            if byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
        }
    }
    Ok(String::from_utf8(line).unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()))
}

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
