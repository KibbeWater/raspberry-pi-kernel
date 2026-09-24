//! Starting other programs, waiting for them, and pipes between them.

extern crate alloc;

use alloc::vec::Vec;
use rustypi_abi::{Errno, Syscall, INPUT, OUTPUT};
use crate::{io, syscall, Handle};

pub use rustypi_abi::ExitStatus;

/// A program this one started. Dropping it without waiting lets it run on, unwatched.
pub struct Child(Handle);

/// A child's input: this program's own, or what comes out of a pipe.
pub enum Input<'a> {
    Inherit,
    Pipe(&'a PipeReader),
}

/// A child's output: this program's own, or into a pipe.
pub enum Output<'a> {
    Inherit,
    Pipe(&'a PipeWriter),
}

/// Starts the program in the file at `path` with `args`, sharing this program's input and
/// output.
pub fn spawn(path: &str, args: &str) -> Result<Child, Errno> {
    spawn_with(path, args, Input::Inherit, Output::Inherit)
}

/// Starts a program with the given input and output. The child gets its own handles on any
/// pipe ends: drop this program's once it doesn't need them, or readers never see the end.
pub fn spawn_with(path: &str, args: &str, input: Input, output: Output) -> Result<Child, Errno> {
    let call = Syscall::Spawn {
        path: path.as_ptr() as u64,
        path_len: path.len() as u64,
        args: args.as_ptr() as u64,
        args_len: args.len() as u64,
        input: match input {
            Input::Inherit => INPUT,
            Input::Pipe(reader) => reader.0 .0,
        },
        output: match output {
            Output::Inherit => OUTPUT,
            Output::Pipe(writer) => writer.0 .0,
        },
    };
    syscall::call(call).map(|handle| Child(Handle(handle)))
}

impl Child {
    /// Waits for it to end. Input this program gets meanwhile goes to the child.
    pub fn wait(self) -> Result<ExitStatus, Errno> {
        // The kernel closes the handle.
        let status = syscall::call(Syscall::Wait { handle: self.0.into_raw() })?;
        ExitStatus::decode(status).ok_or(Errno::Unknown)
    }
}

/// The end of a pipe that bytes come out of.
pub struct PipeReader(Handle);

/// The end of a pipe that bytes go into.
pub struct PipeWriter(Handle);

/// Makes a pipe: what goes into the writer comes out of the reader.
pub fn pipe() -> Result<(PipeReader, PipeWriter), Errno> {
    let mut ends = [0u64; 2];
    syscall::call(Syscall::Pipe { ends: ends.as_mut_ptr() as u64 })?;
    Ok((PipeReader(Handle(ends[0])), PipeWriter(Handle(ends[1]))))
}

impl PipeReader {
    /// Reads into `buf`, waiting until there is something: 0 once every writer has gone.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        io::read_handle(self.0 .0, buf)
    }

    /// Reads until every writer has gone, onto the end of `bytes`.
    pub fn read_to_end(&mut self, bytes: &mut Vec<u8>) -> Result<usize, Errno> {
        io::read_to_end_from(self.0 .0, bytes)
    }
}

impl PipeWriter {
    /// Writes what fits, waiting until something does. `BrokenPipe` once no reader is left.
    pub fn write(&mut self, bytes: &[u8]) -> Result<usize, Errno> {
        io::write_handle(self.0 .0, bytes)
    }

    pub fn write_all(&mut self, bytes: &[u8]) -> Result<(), Errno> {
        io::write_all_to(self.0 .0, bytes)
    }
}
