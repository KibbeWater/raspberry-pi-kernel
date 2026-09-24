// handles.rs
//! What a program has open, by handle: files, directories and the children it started. And
//! the system calls on them.
//!
//! The filesystem is read-only, and files are small, so opening a file reads it whole (up to
//! `MAX_FILE`); reads then come from memory. Opening a directory lists it.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;
use rustypi_abi::layout::MAX_ARGS;
use rustypi_abi::{DirEntry, Errno, ExitStatus, INPUT, MAX_FILE, MAX_HANDLES, MAX_PATH, MAX_READ};
use rustypi_core::elf;
use rustypi_core::fat::{self, EntryKind, FatError};
use crate::sched;
use crate::synchronization::interface::Mutex;
use crate::sys::fs::{self, FsError};
use super::{copy_from_user, was_killed, Code, Exit, Process, SpawnError, PROCESSES};

enum Open {
    File { data: Vec<u8>, position: usize },
    Dir { entries: Vec<fat::DirEntry>, position: usize },
    Child(Process),
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

    fn has_room(&self) -> bool {
        self.0.len() < MAX_HANDLES || self.0.iter().any(Option::is_none)
    }

    fn slot(&mut self, handle: u64) -> Result<&mut Option<Open>, Errno> {
        let slot = handle.checked_sub(1).ok_or(Errno::BadHandle)? as usize;
        self.0.get_mut(slot).filter(|slot| slot.is_some()).ok_or(Errno::BadHandle)
    }

    fn get_mut(&mut self, handle: u64) -> Result<&mut Open, Errno> {
        Ok(self.slot(handle)?.as_mut().expect("slot filters out empty slots"))
    }

    fn take(&mut self, handle: u64) -> Result<Open, Errno> {
        Ok(self.slot(handle)?.take().expect("slot filters out empty slots"))
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

/// Copies UTF-8 text of at most `max` bytes, like a path, out of program memory.
fn user_string(ptr: u64, len: u64, max: usize) -> Result<String, Errno> {
    if len > max as u64 {
        return Err(Errno::Invalid);
    }
    let mut buf = vec![0; len as usize];
    copy_from_user(ptr, &mut buf)?;
    String::from_utf8(buf).map_err(|_| Errno::Invalid)
}

/// Reads a whole file, checking its size first so a huge one can't use up the kernel heap.
fn read_file(path: &str) -> Result<Vec<u8>, Errno> {
    let entry = fs::metadata(path).map_err(errno)?;
    if entry.kind == EntryKind::Directory {
        return Err(Errno::IsADirectory);
    }
    if entry.size as usize > MAX_FILE {
        return Err(Errno::NoMemory);
    }
    fs::read_file(path).map_err(errno)
}

pub(super) fn open(path: u64, len: u64) -> Result<u64, Errno> {
    let data = read_file(&user_string(path, len, MAX_PATH)?)?;
    with_running(|running| running.handles.insert(Open::File { data, position: 0 }))
}

pub(super) fn open_dir(path: u64, len: u64) -> Result<u64, Errno> {
    let path = user_string(path, len, MAX_PATH)?;
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
        Open::Child(_) => Err(Errno::BadHandle),
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
        Open::Child(_) => Err(Errno::BadHandle),
    })
}

/// Closing a child's handle lets it run on, unwatched.
pub(super) fn close(handle: u64) -> Result<u64, Errno> {
    with_running(|running| running.handles.take(handle)).map(|_| 0)
}

pub(super) fn spawn(path: u64, path_len: u64, args: u64, args_len: u64) -> Result<u64, Errno> {
    let path = user_string(path, path_len, MAX_PATH)?;
    let args = user_string(args, args_len, MAX_ARGS)?;
    // Checked first: the child shouldn't start if its handle has nowhere to go. Only this
    // program opens its handles, and it is busy in here.
    if !with_running(|running| running.handles.has_room()) {
        return Err(Errno::TooMany);
    }
    let file = read_file(&path)?;
    let program = elf::parse(&file).map_err(|_| Errno::NotExecutable)?;
    let name = path.rsplit('/').next().unwrap_or(&path);
    let child = super::spawn(name, Code::Elf(&program), &args).map_err(|error| match error {
        SpawnError::TooManyPrograms => Errno::TooMany,
        SpawnError::ArgsTooLong => Errno::Invalid,
        SpawnError::OutOfMemory => Errno::NoMemory,
    })?;
    with_running(|running| running.handles.insert(Open::Child(child)))
}

/// How often `wait` checks on the child.
const WAIT_POLL: Duration = Duration::from_millis(10);

/// Waits for a child to end, passing this program's input on to it meanwhile. A killed
/// program stops waiting (and exits once the call returns); the child runs on.
pub(super) fn wait(handle: u64) -> Result<u64, Errno> {
    let child = with_running(|running| {
        if !matches!(running.handles.get_mut(handle)?, Open::Child(_)) {
            return Err(Errno::BadHandle);
        }
        let Open::Child(child) = running.handles.take(handle)? else { unreachable!() };
        running.waiting_for = Some(child.id());
        Ok(child)
    })?;
    let me = sched::current();
    let exit = loop {
        if let Some(exit) = child.exit() {
            break Some(exit);
        }
        if was_killed(me) {
            break None;
        }
        sched::sleep(WAIT_POLL);
    };
    with_running(|running| running.waiting_for = None);
    let status = match exit {
        Some(Exit::Code(code)) => ExitStatus::Code(code),
        Some(Exit::Crashed(_)) => ExitStatus::Crashed,
        Some(Exit::Killed) | None => ExitStatus::Killed,
    };
    Ok(status.encode())
}
