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
use core::sync::atomic::{AtomicUsize, Ordering};
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

/// A spinlock that also masks IRQs while held, so interrupt handlers on this core can't
/// observe or modify the data halfway through an update, and other cores wait their turn.
/// For short updates only: another core spins meanwhile, with its IRQs masked too.
///
/// Locking it again on the same core, from inside its own closure or from an interrupt
/// handler, would spin forever, so it panics instead. Never take a `TryLock` while holding
/// one: a core spinning for it with IRQs masked could be what the holder waits on.
pub struct IrqLock<T>
where
    T: ?Sized,
{
    /// The holding core plus one; 0 when free.
    owner: AtomicUsize,
    data: UnsafeCell<T>,
}

unsafe impl<T> Send for IrqLock<T> where T: ?Sized + Send {}
unsafe impl<T> Sync for IrqLock<T> where T: ?Sized + Send {}

impl<T> IrqLock<T> {
    /// Create an instance.
    pub const fn new(data: T) -> Self {
        Self {
            owner: AtomicUsize::new(0),
            data: UnsafeCell::new(data),
        }
    }
}

impl<T> interface::Mutex for IrqLock<T> {
    type Data = T;

    fn lock<R>(&self, f: impl FnOnce(&mut Self::Data) -> R) -> R {
        let saved = arch::irq_save();
        let me = arch::core_id() + 1;
        if spin_until_owned(&self.owner, me, || ()).is_err() {
            panic!("IrqLock locked again on core {}", me - 1);
        }
        // Held by this core, with IRQs masked: nothing else can reach the data.
        let result = f(unsafe { &mut *self.data.get() });
        self.owner.store(0, Ordering::Release);
        arch::irq_restore(saved);
        result
    }
}

/// Takes `owner` for `me` (a core plus one), spinning while another core has it. If this core
/// has it already, returns `held_here()` instead.
fn spin_until_owned<R>(owner: &AtomicUsize, me: usize, held_here: impl FnOnce() -> R) -> Result<(), R> {
    loop {
        match owner.compare_exchange_weak(0, me, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => return Ok(()),
            Err(holder) if holder == me => return Err(held_here()),
            Err(_) => core::hint::spin_loop(),
        }
    }
}

/// A lock that never masks IRQs, for data that is slow to update, where masking interrupts
/// would lose UART bytes: the screen, the mailbox, console output. Another core that wants it
/// spins until it is free; on the holder's own core, `try_lock` gives up and returns `None`.
///
/// Holding it disables preemption, so no other task on the holder's core can find it held.
/// Only an interrupt or exception handler that arrived while the holder was running can, and
/// that one always finishes (or never returns) before the holder continues.
pub struct TryLock<T>
where
    T: ?Sized,
{
    /// The holding core plus one; 0 when free.
    owner: AtomicUsize,
    data: UnsafeCell<T>,
}

unsafe impl<T> Send for TryLock<T> where T: ?Sized + Send {}
unsafe impl<T> Sync for TryLock<T> where T: ?Sized + Send {}

impl<T> TryLock<T> {
    /// Create an instance.
    pub const fn new(data: T) -> Self {
        Self {
            owner: AtomicUsize::new(0),
            data: UnsafeCell::new(data),
        }
    }

    /// Runs `f` with the data, or returns `None` without running it if this core holds the
    /// lock already.
    pub fn try_lock<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        // Preemption goes off first, so the core can't change while the lock is held.
        sched::no_preempt(|| {
            let me = arch::core_id() + 1;
            spin_until_owned(&self.owner, me, || ()).ok()?;
            let result = f(unsafe { &mut *self.data.get() });
            self.owner.store(0, Ordering::Release);
            Some(result)
        })
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

        // A holder that hands the lock over before this task blocks leaves it a wake-up, so
        // `block` returns straight away.
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
