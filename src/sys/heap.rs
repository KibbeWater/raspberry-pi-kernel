// heap.rs
//! The kernel heap: `rustypi_core::heap::Heap` over the region `linker.ld` reserves,
//! registered as the global allocator.

use core::alloc::{GlobalAlloc, Layout};
use rustypi_core::heap::{Heap, Stats};
use crate::synchronization::{interface::Mutex, IrqLock};

struct KernelAllocator(IrqLock<Heap>);

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.0.lock(|heap| heap.alloc(layout))
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.0.lock(|heap| unsafe { heap.dealloc(ptr, layout) })
    }
}

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator(IrqLock::new(Heap::empty()));

/// Hands the heap region from `linker.ld` to the allocator. Allocating before this fails.
pub fn init() {
    extern "C" {
        static __heap_start: u8;
        static __heap_end: u8;
    }
    let (start, end) = (&raw const __heap_start as usize, &raw const __heap_end as usize);
    // The linker reserves this region for the heap alone.
    ALLOCATOR.0.lock(|heap| unsafe { heap.init(start, end) });
}

pub fn stats() -> Stats {
    ALLOCATOR.0.lock(|heap| heap.stats())
}
