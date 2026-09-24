// handles.rs
//! What a program has open, by handle: files (to read, or new ones to write), directories,
//! the children it started, pipe ends and the screen. And the system calls on them.
//!
//! The filesystem is read-only, and files are small, so opening a file reads it whole (up to
//! `MAX_FILE`); reads then come from memory. Opening a directory lists it.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;
use rustypi_abi::layout::MAX_ARGS;
use rustypi_abi::{DirEntry, Errno, ExitStatus, INPUT, MAX_HANDLES, MAX_PATH, MAX_READ, OUTPUT};
use rustypi_core::elf;
use rustypi_core::sched::TaskId;
use rustypi_core::fat::{self, EntryKind, FatError};
use crate::sched;
use crate::synchronization::interface::Mutex;
use crate::sys::console::{self, LendError};
use crate::sys::fs::{self, FsError};
use rustypi_abi::ScreenSize;
use super::buffer::FileBuffer;
use super::pipe::{End, PipeEnd};
use super::{copy_from_user, read_pipe, was_killed, write_pipe, Code, Exit, Io, Process, Running, SpawnError, Stream, PROCESSES};

enum Open {
    File { data: FileBuffer, position: usize },
    Dir { entries: Vec<fat::DirEntry>, position: usize },
    Child(Process),
    Pipe(PipeEnd),
    Screen(console::Lease),
    /// A file being written: it all goes to the card when the handle is closed.
    NewFile { path: String, data: FileBuffer },
}

/// The first handle `Handles` gives out; below it are `INPUT` and `OUTPUT`.
const FIRST: u64 = OUTPUT + 1;

/// A program's open handles. Handle `FIRST + n` is slot `n`.
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
        Ok(slot as u64 + FIRST)
    }

    fn free_slots(&self) -> usize {
        MAX_HANDLES - self.0.iter().filter(|slot| slot.is_some()).count()
    }

    /// Whether it holds child `id`'s handle.
    pub(super) fn watches(&self, id: TaskId) -> bool {
        self.0.iter().flatten().any(|open| matches!(open, Open::Child(child) if child.id() == id))
    }

    fn has_room(&self) -> bool {
        self.free_slots() > 0
    }

    fn slot(&mut self, handle: u64) -> Result<&mut Option<Open>, Errno> {
        let slot = handle.checked_sub(FIRST).ok_or(Errno::BadHandle)? as usize;
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
        FsError::Fat(FatError::NoSpace | FatError::DirectoryFull) => Errno::NoSpace,
        FsError::Fat(FatError::AlreadyExists) => Errno::AlreadyExists,
        FsError::Fat(FatError::NotEmpty) => Errno::NotEmpty,
        FsError::Fat(FatError::InvalidName | FatError::TooBig) => Errno::Invalid,
        FsError::Protected => Errno::Protected,
        FsError::TooBig => Errno::NoMemory,
        _ => Errno::Io,
    }
}

/// Runs `f` with the running program's handles and memory.
fn with_running<R>(f: impl FnOnce(&mut Running) -> R) -> R {
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

pub(super) fn open(path: u64, len: u64) -> Result<u64, Errno> {
    let data = FileBuffer::adopt(fs::read_file(&user_string(path, len, MAX_PATH)?).map_err(errno)?)?;
    with_running(|running| running.handles.insert(Open::File { data, position: 0 }))
}

pub(super) fn open_dir(path: u64, len: u64) -> Result<u64, Errno> {
    let path = user_string(path, len, MAX_PATH)?;
    let entries = fs::read_dir(&path).map_err(errno)?;
    with_running(|running| running.handles.insert(Open::Dir { entries, position: 0 }))
}

/// Reads an open file or a pipe's read end. `INPUT` is handled by the caller.
pub(super) fn read(handle: u64, ptr: u64, len: u64) -> Result<u64, Errno> {
    debug_assert_ne!(handle, INPUT);
    // A pipe is waited on outside the lock.
    let pipe = with_running(|running| match running.handles.get_mut(handle)? {
        Open::Pipe(end) if end.end() == End::Read => Ok(Some(end.pipe())),
        Open::Pipe(_) => Err(Errno::BadHandle),
        _ => Ok(None),
    })?;
    if let Some(pipe) = pipe {
        return read_pipe(&pipe, ptr, len);
    }
    with_running(|running| match running.handles.get_mut(handle)? {
        Open::File { data, position } => {
            let count = (len as usize).min(MAX_READ).min(data.len() - *position);
            running.memory.write_user(ptr, &data[*position..*position + count]).map_err(|_| Errno::Fault)?;
            *position += count;
            Ok(count as u64)
        }
        Open::Dir { .. } => Err(Errno::IsADirectory),
        Open::Child(_) | Open::Pipe(_) | Open::Screen(_) | Open::NewFile { .. } => Err(Errno::BadHandle),
    })
}

/// Writes to a pipe's write end or a new file. `OUTPUT` is handled by the caller.
pub(super) fn write(handle: u64, ptr: u64, len: u64) -> Result<u64, Errno> {
    let pipe = with_running(|running| match running.handles.get_mut(handle)? {
        Open::Pipe(end) if end.end() == End::Write => Ok(Some(end.pipe())),
        Open::NewFile { .. } => Ok(None),
        _ => Err(Errno::BadHandle),
    })?;
    if let Some(pipe) = pipe {
        return write_pipe(&pipe, ptr, len);
    }
    // Copied in before taking the lock again to add it.
    let mut buf = [0; MAX_READ];
    let bytes = copy_from_user(ptr, &mut buf[..len.min(MAX_READ as u64) as usize])?;
    with_running(|running| match running.handles.get_mut(handle)? {
        Open::NewFile { data, .. } => {
            data.extend(bytes)?;
            Ok(bytes.len() as u64)
        }
        _ => Err(Errno::BadHandle),
    })
}

/// Starts a new file, to be written to the card when its handle is closed.
pub(super) fn create(path: u64, len: u64) -> Result<u64, Errno> {
    let path = user_string(path, len, MAX_PATH)?;
    fs::check_writable(&path).map_err(errno)?;
    with_running(|running| running.handles.insert(Open::NewFile { path, data: FileBuffer::new() }))
}

pub(super) fn remove(path: u64, len: u64) -> Result<u64, Errno> {
    fs::remove(&user_string(path, len, MAX_PATH)?).map(|()| 0).map_err(errno)
}

pub(super) fn make_dir(path: u64, len: u64) -> Result<u64, Errno> {
    fs::create_dir(&user_string(path, len, MAX_PATH)?).map(|()| 0).map_err(errno)
}

/// Makes a pipe and writes its read and write handles to `ends`.
pub(super) fn pipe(ends: u64) -> Result<u64, Errno> {
    with_running(|running| {
        if running.handles.free_slots() < 2 {
            return Err(Errno::TooMany);
        }
        let (reader, writer) = PipeEnd::pair();
        let read = running.handles.insert(Open::Pipe(reader))?;
        let write = running.handles.insert(Open::Pipe(writer))?;
        let mut handles = [0; 16];
        handles[..8].copy_from_slice(&read.to_le_bytes());
        handles[8..].copy_from_slice(&write.to_le_bytes());
        if running.memory.write_user(ends, &handles).is_err() {
            // Nobody could use them.
            let _ = running.handles.take(read);
            let _ = running.handles.take(write);
            return Err(Errno::Fault);
        }
        Ok(0)
    })
}

/// The stream a child gets for `handle`: this program's own `INPUT` or `OUTPUT`, or a pipe end
/// of the right kind.
fn stream_for(running: &mut Running, handle: u64, end: End) -> Result<Stream, Errno> {
    match (handle, end) {
        (INPUT, End::Read) => Ok(running.io.input.duplicate()),
        (OUTPUT, End::Write) => Ok(running.io.output.duplicate()),
        _ => match running.handles.get_mut(handle)? {
            Open::Pipe(pipe) if pipe.end() == end => Ok(Stream::Pipe(pipe.duplicate())),
            _ => Err(Errno::BadHandle),
        },
    }
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
        Open::Child(_) | Open::Pipe(_) | Open::Screen(_) | Open::NewFile { .. } => Err(Errno::BadHandle),
    })
}

/// Takes the screen from the console, and writes its size to `size`.
pub(super) fn open_screen(size: u64) -> Result<u64, Errno> {
    with_running(|running| {
        if !running.handles.has_room() {
            return Err(Errno::TooMany);
        }
        let lease = console::lend().map_err(|error| match error {
            LendError::NoScreen => Errno::NoDevice,
            LendError::Busy => Errno::Busy,
        })?;
        let (width, height) = lease.size();
        let bytes = ScreenSize { width: width as u32, height: height as u32 }.as_bytes();
        // On failure the lease is dropped here, and the console gets the screen back.
        running.memory.write_user(size, &bytes).map_err(|_| Errno::Fault)?;
        running.handles.insert(Open::Screen(lease))
    })
}

/// Draws a rectangle of the running program's pixels on the screen it holds, a row at a time.
pub(super) fn draw(handle: u64, x: u64, y: u64, width: u64, height: u64, pixels: u64) -> Result<u64, Errno> {
    let (screen_width, screen_height) = with_running(|running| match running.handles.get_mut(handle)? {
        Open::Screen(lease) => Ok(lease.size()),
        _ => Err(Errno::BadHandle),
    })?;
    // Only the part on the screen is copied in.
    let (x, y) = (x.min(screen_width as u64) as usize, y.min(screen_height as u64) as usize);
    let visible_width = (width as usize).min(screen_width - x);
    let visible_height = (height as usize).min(screen_height - y);
    let row_bytes = width.checked_mul(4).ok_or(Errno::Invalid)?;
    let mut row = vec![0u8; visible_width * 4];
    let mut words = vec![0u32; visible_width];
    for r in 0..visible_height {
        let at = pixels.checked_add(r as u64 * row_bytes).ok_or(Errno::Fault)?;
        copy_from_user(at, &mut row)?;
        for (word, bytes) in words.iter_mut().zip(row.chunks_exact(4)) {
            *word = u32::from_le_bytes(bytes.try_into().unwrap());
        }
        // The handle might have been closed meanwhile only by this program, which is in here.
        with_running(|running| match running.handles.get_mut(handle) {
            Ok(Open::Screen(lease)) => {
                lease.draw_row(x, y + r, &words);
                Ok(())
            }
            _ => Err(Errno::BadHandle),
        })?;
    }
    Ok(0)
}

/// Closing a new file writes it to the card; closing a child's handle lets it run on,
/// unwatched. Whatever needs undoing happens here, outside the lock (like the screen coming
/// back to the console).
pub(super) fn close(handle: u64) -> Result<u64, Errno> {
    match with_running(|running| running.handles.take(handle))? {
        Open::NewFile { path, data } => fs::write_file(&path, &data).map(|()| 0).map_err(errno),
        _ => Ok(0),
    }
}

pub(super) fn spawn(path: u64, path_len: u64, args: u64, args_len: u64, input: u64, output: u64) -> Result<u64, Errno> {
    let path = user_string(path, path_len, MAX_PATH)?;
    let args = user_string(args, args_len, MAX_ARGS)?;
    // Checked first: the child shouldn't start if its handle has nowhere to go. Only this
    // program opens its handles, and it is busy in here.
    let io = with_running(|running| {
        if !running.handles.has_room() {
            return Err(Errno::TooMany);
        }
        Ok(Io { input: stream_for(running, input, End::Read)?, output: stream_for(running, output, End::Write)? })
    })?;
    let file = FileBuffer::adopt(fs::read_file(&path).map_err(errno)?)?;
    let program = elf::parse(&file).map_err(|_| Errno::NotExecutable)?;
    let name = path.rsplit('/').next().unwrap_or(&path);
    let parent = sched::current();
    let child = super::spawn_with(name, Code::Elf(&program), &args, io, Some(parent)).map_err(|error| match error {
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
