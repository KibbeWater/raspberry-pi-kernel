//! Starting other programs and waiting for them.

use rustypi_abi::{Errno, Syscall};
use crate::{syscall, Handle};

pub use rustypi_abi::ExitStatus;

/// A program this one started. Dropping it without waiting lets it run on, unwatched.
pub struct Child(Handle);

/// Starts the program in the file at `path` with `args`.
pub fn spawn(path: &str, args: &str) -> Result<Child, Errno> {
    let call = Syscall::Spawn {
        path: path.as_ptr() as u64,
        path_len: path.len() as u64,
        args: args.as_ptr() as u64,
        args_len: args.len() as u64,
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
