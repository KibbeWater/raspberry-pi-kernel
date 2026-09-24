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
use crate::arch;

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
/// and returns `None`. For data that is slow to update, where losing an update is better than
/// blocking interrupts, like the screen.
///
/// Only sound while the kernel runs on a single core. There, the only way to find it held is
/// from an interrupt or exception that arrived while the holder was running, and that one
/// always finishes (or never returns) before the holder continues.
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
        self.locked.store(true, Ordering::Release);
        let result = f(unsafe { &mut *self.data.get() });
        self.locked.store(false, Ordering::Release);
        Some(result)
    }
}
