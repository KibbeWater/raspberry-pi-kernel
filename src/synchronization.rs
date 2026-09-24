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
