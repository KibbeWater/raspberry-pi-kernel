//! Files and directories on the SD card. Paths start at its root, with or without a leading
//! `/`. Files the Pi needs to boot (firmware, `config.txt`, kernel, device trees, overlays)
//! can be read but not changed: that's `Protected`.

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

/// A file being written. It reaches the card all at once when committed (or dropped, which
/// can't report errors), replacing any file there; until then the old one stays as it was.
pub struct NewFile(Handle);

impl NewFile {
    /// Starts a new file at `path`. Its directory must exist.
    pub fn create(path: &str) -> Result<NewFile, Errno> {
        let call = Syscall::Create { path: path.as_ptr() as u64, len: path.len() as u64 };
        syscall::call(call).map(|handle| NewFile(Handle(handle)))
    }

    /// Adds some of `bytes`, returning how many.
    pub fn write(&mut self, bytes: &[u8]) -> Result<usize, Errno> {
        crate::io::write_handle(self.0 .0, bytes)
    }

    pub fn write_all(&mut self, mut bytes: &[u8]) -> Result<(), Errno> {
        while !bytes.is_empty() {
            let written = self.write(bytes)?;
            bytes = &bytes[written..];
        }
        Ok(())
    }

    /// Writes the file to the card.
    pub fn commit(self) -> Result<(), Errno> {
        syscall::call(Syscall::Close { handle: self.0.into_raw() }).map(|_| ())
    }
}

/// Creates or replaces the file at `path` with `bytes`.
pub fn write(path: &str, bytes: &[u8]) -> Result<(), Errno> {
    let mut file = NewFile::create(path)?;
    file.write_all(bytes)?;
    file.commit()
}

/// Removes a file or an empty directory.
pub fn remove(path: &str) -> Result<(), Errno> {
    syscall::call(Syscall::Remove { path: path.as_ptr() as u64, len: path.len() as u64 }).map(|_| ())
}

pub fn create_dir(path: &str) -> Result<(), Errno> {
    syscall::call(Syscall::MakeDir { path: path.as_ptr() as u64, len: path.len() as u64 }).map(|_| ())
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
