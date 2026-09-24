// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) 2020-2023 Andre Richter <andre.o.richter@gmail.com>

//! Synchronization primitives.
//!
//! # Resources
//!
//!   - <https://doc.rust-lang.org/book/ch16-04-extensible-concurrency-sync-and-send.html>
//!   - <https://stackoverflow.com/questions/59428096/understanding-the-send-trait>
//!   - <https://doc.rust-lang.org/std/cell/index.html>

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};
use rustypi_core::lock::{Acquire, LockState};
use crate::{arch, sched};

/// Synchronization interfaces.
pub mod interface {

    /// Any object implementing this trait guarantees exclusive access to the data wrapped within
    /// the Mutex for the duration of the provided closure.
    pub trait Mutex {
        /// The type of the data that is wrapped by this mutex.
        type Data;

        /// Locks the mutex and grants the closure temporary mutable access to the wrapped data.
        fn lock<R>(&self, f: impl FnOnce(&mut Self::Data) -> R) -> R;
    }
}

/// A lock that masks IRQs while held, so interrupt handlers can't observe or modify the data
/// halfway through an update.
///
/// Only sound while the kernel runs on a single core; other cores would need a spinlock on
/// top. Must not be locked again from inside its own closure.
pub struct IrqLock<T>
where
    T: ?Sized,
{
    data: UnsafeCell<T>,
}

unsafe impl<T> Send for IrqLock<T> where T: ?Sized + Send {}
unsafe impl<T> Sync for IrqLock<T> where T: ?Sized + Send {}

impl<T> IrqLock<T> {
    /// Create an instance.
    pub const fn new(data: T) -> Self {
        Self {
            data: UnsafeCell::new(data),
        }
    }
}

impl<T> interface::Mutex for IrqLock<T> {
    type Data = T;

    fn lock<R>(&self, f: impl FnOnce(&mut Self::Data) -> R) -> R {
        let saved = arch::irq_save();
        // With IRQs masked on the only running core, nothing else can reach the data.
        let result = f(unsafe { &mut *self.data.get() });
        arch::irq_restore(saved);
        result
    }
}

/// A lock that never waits and never masks IRQs: if it is already held, `try_lock` gives up
/// and returns `None`. For data that is slow to update, where blocking interrupts would lose
/// UART bytes, like the screen or the SD card.
///
/// Holding it disables preemption, so no other task can find it held. Only an interrupt or
/// exception handler that arrived while the holder was running can, and that one always
/// finishes (or never returns) before the holder continues. Only sound on a single core.
pub struct TryLock<T>
where
    T: ?Sized,
{
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T> Send for TryLock<T> where T: ?Sized + Send {}
unsafe impl<T> Sync for TryLock<T> where T: ?Sized + Send {}

impl<T> TryLock<T> {
    /// Create an instance.
    pub const fn new(data: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    /// Runs `f` with the data, or returns `None` without running it if the lock is held.
    pub fn try_lock<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        if self.locked.load(Ordering::Acquire) {
            return None;
        }
        Some(sched::no_preempt(|| {
            self.locked.store(true, Ordering::Release);
            let result = f(unsafe { &mut *self.data.get() });
            self.locked.store(false, Ordering::Release);
            result
        }))
    }
}

/// A sleeping lock for data that takes a while to use, like the SD card: a task that finds it
/// held blocks until the holder hands it over, while other tasks keep running. Unlike
/// `TryLock`, holding it doesn't stop preemption.
///
/// Blocking needs task code with IRQs enabled. Finding it held anywhere else (an interrupt
/// handler, or with IRQs masked) panics rather than deadlocking; an uncontended lock works
/// anywhere, including early boot. Locking it again from inside its own closure panics too,
/// rather than waiting forever.
pub struct Mutex<T>
where
    T: ?Sized,
{
    state: IrqLock<LockState>,
    data: UnsafeCell<T>,
}

unsafe impl<T> Send for Mutex<T> where T: ?Sized + Send {}
unsafe impl<T> Sync for Mutex<T> where T: ?Sized + Send {}

impl<T> Mutex<T> {
    /// Create an instance.
    pub const fn new(data: T) -> Self {
        Self {
            state: IrqLock::new(LockState::new()),
            data: UnsafeCell::new(data),
        }
    }
}

impl<T> interface::Mutex for Mutex<T> {
    type Data = T;

    fn lock<R>(&self, f: impl FnOnce(&mut Self::Data) -> R) -> R {
        let can_block = arch::irqs_enabled();
        let me = sched::current();

        // With IRQs masked, the holder can't run (and hand the lock over) between queueing
        // and blocking.
        let saved = arch::irq_save();
        match self.state.lock(|state| state.lock(me)) {
            Ok(Acquire::Acquired) => {}
            Ok(Acquire::Queued) if !can_block => {
                panic!("sleeping Mutex held elsewhere, and this code can't block (IRQs masked)");
            }
            Ok(Acquire::Queued) => {
                sched::block();
                debug_assert_eq!(self.state.lock(|state| state.owner()), Some(me));
            }
            Err(error) => panic!("Mutex::lock: {:?}", error),
        }
        arch::irq_restore(saved);

        // This task owns the lock until it unlocks below.
        let result = f(unsafe { &mut *self.data.get() });

        match self.state.lock(|state| state.unlock(me)) {
            Ok(Some(next)) => sched::wake(next),
            Ok(None) => {}
            Err(error) => panic!("Mutex::unlock: {:?}", error),
        }
        result
    }
}
