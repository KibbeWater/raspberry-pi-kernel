// files.rs
//! The files and directories a program has open, by handle, and the system calls on them.
//!
//! The filesystem is read-only, and files are small, so opening a file reads it whole (up to
//! `MAX_FILE`); reads then come from memory. Opening a directory lists it.

use alloc::vec::Vec;
use rustypi_abi::{DirEntry, Errno, INPUT, MAX_FILE, MAX_HANDLES, MAX_PATH, MAX_READ};
use rustypi_core::fat::{self, EntryKind, FatError};
use crate::sched;
use crate::synchronization::interface::Mutex;
use crate::sys::fs::{self, FsError};
use super::{copy_from_user, PROCESSES};

enum Open {
    File { data: Vec<u8>, position: usize },
    Dir { entries: Vec<fat::DirEntry>, position: usize },
}

/// A program's open handles. Handle `n` is slot `n - 1`: 0 is `INPUT`, which isn't here.
pub(super) struct Handles(Vec<Option<Open>>);

impl Handles {
    pub(super) fn new() -> Self {
        Handles(Vec::new())
    }

    fn insert(&mut self, open: Open) -> Result<u64, Errno> {
        let slot = match self.0.iter().position(Option::is_none) {
            Some(slot) => slot,
            None if self.0.len() < MAX_HANDLES => {
                self.0.push(None);
                self.0.len() - 1
            }
            None => return Err(Errno::TooMany),
        };
        self.0[slot] = Some(open);
        Ok(slot as u64 + 1)
    }

    fn get_mut(&mut self, handle: u64) -> Result<&mut Open, Errno> {
        let slot = handle.checked_sub(1).ok_or(Errno::BadHandle)? as usize;
        self.0.get_mut(slot).and_then(Option::as_mut).ok_or(Errno::BadHandle)
    }
}

fn errno(error: FsError) -> Errno {
    match error {
        FsError::Fat(FatError::NotFound) => Errno::NotFound,
        FsError::Fat(FatError::NotAFile) => Errno::IsADirectory,
        FsError::Fat(FatError::NotADirectory) => Errno::NotADirectory,
        _ => Errno::Io,
    }
}

/// Runs `f` with the running program's handles and memory.
fn with_running<R>(f: impl FnOnce(&mut super::Running) -> R) -> R {
    let id = sched::current();
    PROCESSES.lock(|processes| f(processes.get_mut(&id).expect("a user task has a process")))
}

/// Copies a path out of program memory.
fn user_path(ptr: u64, len: u64) -> Result<alloc::string::String, Errno> {
    if len > MAX_PATH as u64 {
        return Err(Errno::Invalid);
    }
    let mut buf = [0; MAX_PATH];
    let bytes = copy_from_user(ptr, &mut buf[..len as usize])?;
    core::str::from_utf8(bytes).map(Into::into).map_err(|_| Errno::Invalid)
}

pub(super) fn open(path: u64, len: u64) -> Result<u64, Errno> {
    let path = user_path(path, len)?;
    // Checked first, so a huge file can't use up the kernel heap.
    let entry = fs::metadata(&path).map_err(errno)?;
    if entry.kind == EntryKind::Directory {
        return Err(Errno::IsADirectory);
    }
    if entry.size as usize > MAX_FILE {
        return Err(Errno::NoMemory);
    }
    let data = fs::read_file(&path).map_err(errno)?;
    with_running(|running| running.handles.insert(Open::File { data, position: 0 }))
}

pub(super) fn open_dir(path: u64, len: u64) -> Result<u64, Errno> {
    let path = user_path(path, len)?;
    let entries = fs::read_dir(&path).map_err(errno)?;
    with_running(|running| running.handles.insert(Open::Dir { entries, position: 0 }))
}

/// Reads an open file. `INPUT` is handled by the caller.
pub(super) fn read(handle: u64, ptr: u64, len: u64) -> Result<u64, Errno> {
    debug_assert_ne!(handle, INPUT);
    with_running(|running| match running.handles.get_mut(handle)? {
        Open::File { data, position } => {
            let count = (len as usize).min(MAX_READ).min(data.len() - *position);
            running.memory.write_user(ptr, &data[*position..*position + count]).map_err(|_| Errno::Fault)?;
            *position += count;
            Ok(count as u64)
        }
        Open::Dir { .. } => Err(Errno::IsADirectory),
    })
}

pub(super) fn read_dir(handle: u64, entry: u64) -> Result<u64, Errno> {
    with_running(|running| match running.handles.get_mut(handle)? {
        Open::Dir { entries, position } => {
            let Some(next) = entries.get(*position) else { return Ok(0) };
            let next = DirEntry::new(&next.name, next.kind == EntryKind::Directory, next.size as u64);
            running.memory.write_user(entry, next.as_bytes()).map_err(|_| Errno::Fault)?;
            *position += 1;
            Ok(1)
        }
        Open::File { .. } => Err(Errno::NotADirectory),
    })
}

pub(super) fn close(handle: u64) -> Result<u64, Errno> {
    with_running(|running| {
        running.handles.get_mut(handle)?;
        running.handles.0[handle as usize - 1] = None;
        Ok(0)
    })
}
