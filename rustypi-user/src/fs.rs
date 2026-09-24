//! Reading files and directories on the SD card. Paths start at its root, with or without a
//! leading `/`. The card is read-only for programs.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use rustypi_abi::{Errno, Syscall, MAX_READ};
use crate::{syscall, Handle};

pub use rustypi_abi::DirEntry;

/// A file open for reading.
pub struct File(Handle);

impl File {
    pub fn open(path: &str) -> Result<File, Errno> {
        let call = Syscall::Open { path: path.as_ptr() as u64, len: path.len() as u64 };
        syscall::call(call).map(|handle| File(Handle(handle)))
    }

    /// Reads into `buf`, returning how many bytes were read: 0 at the end of the file.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        let call = Syscall::Read { handle: self.0 .0, ptr: buf.as_mut_ptr() as u64, len: buf.len() as u64 };
        syscall::call(call).map(|read| read as usize)
    }

    /// Reads the rest of the file onto the end of `bytes`.
    pub fn read_to_end(&mut self, bytes: &mut Vec<u8>) -> Result<usize, Errno> {
        let mut total = 0;
        let mut chunk = [0; MAX_READ];
        loop {
            let read = self.read(&mut chunk)?;
            if read == 0 {
                return Ok(total);
            }
            bytes.extend_from_slice(&chunk[..read]);
            total += read;
        }
    }
}

/// A whole file.
pub fn read(path: &str) -> Result<Vec<u8>, Errno> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// A whole text file. `Invalid` if it isn't UTF-8.
pub fn read_to_string(path: &str) -> Result<String, Errno> {
    String::from_utf8(read(path)?).map_err(|_| Errno::Invalid)
}

/// The entries of a directory, in the order they are stored.
pub struct ReadDir(Handle);

pub fn read_dir(path: &str) -> Result<ReadDir, Errno> {
    let call = Syscall::OpenDir { path: path.as_ptr() as u64, len: path.len() as u64 };
    syscall::call(call).map(|handle| ReadDir(Handle(handle)))
}

impl Iterator for ReadDir {
    type Item = Result<DirEntry, Errno>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut entry = DirEntry::EMPTY;
        let call = Syscall::ReadDir { handle: self.0 .0, entry: &mut entry as *mut DirEntry as u64 };
        match syscall::call(call) {
            Ok(0) => None,
            Ok(_) => Some(Ok(entry)),
            Err(error) => Some(Err(error)),
        }
    }
}
