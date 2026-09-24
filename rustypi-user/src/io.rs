//! Console output, and input typed while the program runs in the foreground.
//!
//! A prompt can go without a newline (`print!("> ")`): the bridge shows text that stops short
//! after a moment.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use rustypi_abi::{Errno, Syscall, INPUT, MAX_WRITE, OUTPUT};
use crate::syscall;

/// Reads input into `buf`, waiting until there is some. Returns how many bytes were read: 0
/// once the input is over. Typed input comes a line at a time, each ending with `\n`.
pub fn read(buf: &mut [u8]) -> Result<usize, Errno> {
    read_handle(INPUT, buf)
}

/// Waits for the next line of input, and returns it without its `\n`. `None` once the input
/// is over: a pipe whose writers have all gone. (Typed input never ends.)
pub fn read_line() -> Result<Option<String>, Errno> {
    let mut line = Vec::new();
    let mut byte = [0];
    // A byte at a time, so nothing past the line is taken from the kernel's queue.
    loop {
        if read(&mut byte)? == 0 {
            if line.is_empty() {
                return Ok(None);
            }
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    Ok(Some(String::from_utf8(line).unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())))
}

/// Reads all of the input, until it is over.
pub fn read_to_end(bytes: &mut Vec<u8>) -> Result<usize, Errno> {
    let mut chunk = [0; rustypi_abi::MAX_READ];
    let mut total = 0;
    loop {
        let read = read(&mut chunk)?;
        if read == 0 {
            return Ok(total);
        }
        bytes.extend_from_slice(&chunk[..read]);
        total += read;
    }
}

/// Writes bytes to the output (the console, or a pipe), returning how many went.
pub fn write(bytes: &[u8]) -> Result<usize, Errno> {
    write_handle(OUTPUT, bytes)
}

pub(crate) fn write_handle(handle: u64, bytes: &[u8]) -> Result<usize, Errno> {
    let call = Syscall::Write { handle, ptr: bytes.as_ptr() as u64, len: bytes.len() as u64 };
    syscall::call(call).map(|written| written as usize)
}

pub(crate) fn read_handle(handle: u64, buf: &mut [u8]) -> Result<usize, Errno> {
    let call = Syscall::Read { handle, ptr: buf.as_mut_ptr() as u64, len: buf.len() as u64 };
    syscall::call(call).map(|read| read as usize)
}

/// Writes all of `bytes` to the output.
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
