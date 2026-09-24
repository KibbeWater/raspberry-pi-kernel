// lock.rs
//! Ownership bookkeeping for a sleeping mutex: who holds it, and who is waiting, in order.
//! The kernel's `Mutex` blocks and wakes tasks according to what this says.

use alloc::collections::VecDeque;
use crate::sched::TaskId;

#[derive(Debug, PartialEq, Eq)]
pub enum Acquire {
    /// The task holds the lock now.
    Acquired,
    /// The lock is held; the task was queued and must block until it is handed the lock.
    Queued,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LockError {
    /// The task already holds the lock; waiting for it would wait forever.
    AlreadyHeld,
    /// Unlocked by a task that doesn't hold it.
    NotOwner,
}

pub struct LockState {
    owner: Option<TaskId>,
    waiters: VecDeque<TaskId>,
}

impl LockState {
    pub const fn new() -> Self {
        LockState { owner: None, waiters: VecDeque::new() }
    }

    pub fn owner(&self) -> Option<TaskId> {
        self.owner
    }

    pub fn lock(&mut self, task: TaskId) -> Result<Acquire, LockError> {
        match self.owner {
            None => {
                self.owner = Some(task);
                Ok(Acquire::Acquired)
            }
            Some(owner) if owner == task => Err(LockError::AlreadyHeld),
            Some(_) => {
                self.waiters.push_back(task);
                Ok(Acquire::Queued)
            }
        }
    }

    /// Releases the lock, handing it straight to the longest waiter, which is returned so it
    /// can be woken. Handing over (rather than letting everyone race for it) means a task
    /// that keeps relocking can't starve the others.
    pub fn unlock(&mut self, task: TaskId) -> Result<Option<TaskId>, LockError> {
        if self.owner != Some(task) {
            return Err(LockError::NotOwner);
        }
        self.owner = self.waiters.pop_front();
        Ok(self.owner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: TaskId = TaskId(1);
    const B: TaskId = TaskId(2);
    const C: TaskId = TaskId(3);

    #[test]
    fn free_locks_are_acquired_at_once() {
        let mut lock = LockState::new();
        assert_eq!(lock.lock(A), Ok(Acquire::Acquired));
        assert_eq!(lock.owner(), Some(A));
        assert_eq!(lock.unlock(A), Ok(None));
        assert_eq!(lock.owner(), None);
    }

    #[test]
    fn waiters_are_handed_the_lock_in_arrival_order() {
        let mut lock = LockState::new();
        lock.lock(A).unwrap();
        assert_eq!(lock.lock(B), Ok(Acquire::Queued));
        assert_eq!(lock.lock(C), Ok(Acquire::Queued));
        assert_eq!(lock.unlock(A), Ok(Some(B)));
        assert_eq!(lock.owner(), Some(B));
        // A relocking now queues behind C instead of jumping ahead.
        assert_eq!(lock.lock(A), Ok(Acquire::Queued));
        assert_eq!(lock.unlock(B), Ok(Some(C)));
        assert_eq!(lock.unlock(C), Ok(Some(A)));
        assert_eq!(lock.unlock(A), Ok(None));
    }

    #[test]
    fn relocking_and_foreign_unlocks_are_errors() {
        let mut lock = LockState::new();
        lock.lock(A).unwrap();
        assert_eq!(lock.lock(A), Err(LockError::AlreadyHeld));
        assert_eq!(lock.unlock(B), Err(LockError::NotOwner));
        assert_eq!(lock.owner(), Some(A));
    }
}
