// heap.rs
//! Kernel heap: a first-fit free-list allocator over the region `linker.ld` reserves.
//!
//! Free blocks form a singly linked list sorted by address, with each block's header
//! stored in the free memory itself. Freed blocks are merged with free neighbours, so the
//! heap doesn't fragment into unusable slivers. Allocations carry no header: `dealloc`
//! works the size out again from the `Layout`.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;
use crate::synchronization::{interface::Mutex, IrqLock};

/// Every block is a multiple of this in size and aligned to it. It is also the smallest
/// block, since a free block has to hold its header.
const BLOCK_ALIGN: usize = 16;

struct FreeBlock {
    size: usize,
    next: *mut FreeBlock,
}

const _: () = assert!(size_of::<FreeBlock>() <= BLOCK_ALIGN);

pub struct Stats {
    pub total: usize,
    pub used: usize,
    pub largest_free: usize,
}

struct Heap {
    head: *mut FreeBlock,
    total: usize,
    used: usize,
}

// The raw pointers only ever point into the heap region, which the heap owns.
unsafe impl Send for Heap {}

const fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

fn block_size(layout: Layout) -> usize {
    align_up(layout.size().max(1), BLOCK_ALIGN)
}

impl Heap {
    const fn empty() -> Self {
        Heap { head: null_mut(), total: 0, used: 0 }
    }

    /// Takes over `start..end`, which nothing else may use.
    unsafe fn init(&mut self, start: usize, end: usize) {
        let start = align_up(start, BLOCK_ALIGN);
        let end = end & !(BLOCK_ALIGN - 1);
        self.head = null_mut();
        self.total = end - start;
        self.used = 0;
        unsafe { self.free(start, end - start) };
    }

    unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let size = block_size(layout);
        let align = layout.align().max(BLOCK_ALIGN);

        // `link` is the pointer that leads to `block`: the head, or the previous `next`.
        let mut link: *mut *mut FreeBlock = &mut self.head;
        unsafe {
            while !(*link).is_null() {
                let block = *link;
                let block_start = block as usize;
                let block_end = block_start + (*block).size;
                let start = align_up(block_start, align);

                if start + size <= block_end {
                    // Give back what's left after the allocation...
                    let mut rest = (*block).next;
                    let end = start + size;
                    if end < block_end {
                        let tail = end as *mut FreeBlock;
                        tail.write(FreeBlock { size: block_end - end, next: rest });
                        rest = tail;
                    }
                    // ...and before it, when aligning skipped some of the block.
                    if start > block_start {
                        (*block).size = start - block_start;
                        (*block).next = rest;
                    } else {
                        *link = rest;
                    }
                    self.used += size;
                    return start as *mut u8;
                }
                link = &mut (*block).next;
            }
        }
        null_mut()
    }

    unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) {
        let size = block_size(layout);
        unsafe { self.free(ptr as usize, size) };
        self.used -= size;
    }

    /// Returns `start..start + size` to the free list, merging it with its neighbours.
    unsafe fn free(&mut self, start: usize, size: usize) {
        unsafe {
            // Find the free blocks right before and after the freed range.
            let mut prev: *mut FreeBlock = null_mut();
            let mut next = self.head;
            while !next.is_null() && (next as usize) < start {
                prev = next;
                next = (*next).next;
            }

            let end = start + size;
            let prev_end = if prev.is_null() { 0 } else { prev as usize + (*prev).size };
            if start < prev_end || (!next.is_null() && end > next as usize) {
                panic!("heap: freeing {:#x}..{:#x}, which is already free", start, end);
            }

            let mut size = size;
            let mut after = next;
            if !next.is_null() && end == next as usize {
                size += (*next).size;
                after = (*next).next;
            }

            if !prev.is_null() && prev_end == start {
                (*prev).size += size;
                (*prev).next = after;
            } else {
                let block = start as *mut FreeBlock;
                block.write(FreeBlock { size, next: after });
                if prev.is_null() {
                    self.head = block;
                } else {
                    (*prev).next = block;
                }
            }
        }
    }

    fn stats(&self) -> Stats {
        let mut largest_free = 0;
        let mut block = self.head;
        while !block.is_null() {
            unsafe {
                largest_free = largest_free.max((*block).size);
                block = (*block).next;
            }
        }
        Stats { total: self.total, used: self.used, largest_free }
    }
}

struct KernelAllocator(IrqLock<Heap>);

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.0.lock(|heap| unsafe { heap.alloc(layout) })
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
    ALLOCATOR.0.lock(|heap| unsafe { heap.init(start, end) });
}

pub fn stats() -> Stats {
    ALLOCATOR.0.lock(|heap| heap.stats())
}
