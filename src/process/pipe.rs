// pipe.rs
//! Pipes: a bounded queue of bytes from one program's output to another's input.
//!
//! Each end is counted, so a reader knows the input is over once every writer has gone
//! (closed its handle or exited), and a writer knows nobody will read once every reader has.
//! Readers wait while the pipe is empty, writers while it's full, woken through
//! `sched::PIPE`.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use rustypi_abi::{Errno, PIPE_CAPACITY};
use crate::sched;
use crate::synchronization::{interface::Mutex, IrqLock};

struct State {
    bytes: VecDeque<u8>,
    readers: usize,
    writers: usize,
}

type Shared = Arc<IrqLock<State>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum End {
    Read,
    Write,
}

/// One end of a pipe, counted while it exists.
pub(super) struct PipeEnd {
    shared: Shared,
    end: End,
}

impl PipeEnd {
    /// A new pipe's read and write ends.
    pub(super) fn pair() -> (PipeEnd, PipeEnd) {
        let shared = Arc::new(IrqLock::new(State { bytes: VecDeque::new(), readers: 1, writers: 1 }));
        (PipeEnd { shared: shared.clone(), end: End::Read }, PipeEnd { shared, end: End::Write })
    }

    pub(super) fn end(&self) -> End {
        self.end
    }

    /// Another handle on the same end, like a child's `INPUT` or `OUTPUT`.
    pub(super) fn duplicate(&self) -> PipeEnd {
        self.shared.lock(|state| match self.end {
            End::Read => state.readers += 1,
            End::Write => state.writers += 1,
        });
        PipeEnd { shared: self.shared.clone(), end: self.end }
    }

    /// The pipe itself, to wait on without holding the end (or the process it belongs to).
    pub(super) fn pipe(&self) -> Pipe {
        Pipe(self.shared.clone())
    }
}

impl Drop for PipeEnd {
    fn drop(&mut self) {
        self.shared.lock(|state| match self.end {
            End::Read => state.readers -= 1,
            End::Write => state.writers -= 1,
        });
        // Whoever waits on the other end may be done waiting.
        sched::notify(sched::PIPE);
    }
}

/// A pipe, for reading or writing through an end the caller has.
pub(super) struct Pipe(Shared);

impl Pipe {
    /// Takes up to `buf.len()` bytes, waiting while the pipe is empty and has writers. 0 once
    /// it is empty with no writers left, or if `stop()` says to give up.
    pub(super) fn read(&self, buf: &mut [u8], stop: impl Fn() -> bool) -> usize {
        sched::wait_until(sched::PIPE, || stop() || self.0.lock(|state| !state.bytes.is_empty() || state.writers == 0));
        let count = self.0.lock(|state| {
            let count = buf.len().min(state.bytes.len());
            for (slot, byte) in buf.iter_mut().zip(state.bytes.drain(..count)) {
                *slot = byte;
            }
            count
        });
        if count > 0 {
            sched::notify(sched::PIPE);
        }
        count
    }

    /// Adds as much of `bytes` as fits, waiting while the pipe is full and has readers.
    /// `BrokenPipe` once no readers are left; 0 if `stop()` says to give up.
    pub(super) fn write(&self, bytes: &[u8], stop: impl Fn() -> bool) -> Result<usize, Errno> {
        sched::wait_until(sched::PIPE, || {
            stop() || self.0.lock(|state| state.bytes.len() < PIPE_CAPACITY || state.readers == 0)
        });
        let count = self.0.lock(|state| {
            if state.readers == 0 {
                return Err(Errno::BrokenPipe);
            }
            let count = bytes.len().min(PIPE_CAPACITY - state.bytes.len());
            state.bytes.extend(&bytes[..count]);
            Ok(count)
        })?;
        if count > 0 {
            sched::notify(sched::PIPE);
        }
        Ok(count)
    }
}
