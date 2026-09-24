//! The program's environment: its current directory, which relative paths start from.

extern crate alloc;

use alloc::string::String;
use rustypi_abi::{Errno, Syscall, MAX_PATH};
use crate::syscall;

/// The current directory, an absolute path like `/docs`.
pub fn current_dir() -> Result<String, Errno> {
    let mut buf = [0; MAX_PATH];
    let len = syscall::call(Syscall::CurrentDir { buf: buf.as_mut_ptr() as u64, len: buf.len() as u64 })?;
    String::from_utf8(buf[..len as usize].into()).map_err(|_| Errno::Invalid)
}

/// Changes the current directory, for this program and the programs it starts from now on.
pub fn set_current_dir(path: &str) -> Result<(), Errno> {
    syscall::call(Syscall::ChangeDir { path: path.as_ptr() as u64, len: path.len() as u64 }).map(|_| ())
}
