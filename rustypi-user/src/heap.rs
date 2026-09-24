//! The program's heap: `rustypi_core`'s allocator over memory from the `Map` system call,
//! grown whenever an allocation doesn't fit.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use rustypi_abi::layout::PAGE_SIZE;
use rustypi_abi::Syscall;
use rustypi_core::heap::Heap;
use crate::syscall;

pub use rustypi_core::heap::Stats;

/// The heap grows by at least this much at a time, so small allocations don't each need a
/// system call.
const GROW_BY: usize = 64 * 1024;

struct Allocator(UnsafeCell<Heap>);

// A program has a single thread, and nothing interrupts it at EL0 to run program code
// (there are no signals), so the heap is never used from two places at once.
unsafe impl Sync for Allocator {}

#[global_allocator]
static ALLOCATOR: Allocator = Allocator(UnsafeCell::new(Heap::empty()));

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let heap = unsafe { &mut *self.0.get() };
        let block = heap.alloc(layout);
        if !block.is_null() {
            return block;
        }
        // Enough for the block wherever alignment puts it. New memory follows on from the
        // last, so it merges with any free block at the end of the heap.
        let len = (layout.size() + layout.align()).max(GROW_BY).next_multiple_of(PAGE_SIZE as usize);
        match syscall::call(Syscall::Map { len: len as u64 }) {
            Ok(start) => {
                unsafe { heap.extend(start as usize, start as usize + len) };
                heap.alloc(layout)
            }
            Err(_) => core::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        unsafe { (*self.0.get()).dealloc(block, layout) };
    }
}

/// How big the heap has grown, and how much of it is in use.
pub fn stats() -> Stats {
    unsafe { (*ALLOCATOR.0.get()).stats() }
}
