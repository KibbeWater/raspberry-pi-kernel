// heap.rs
//! A first-fit free-list heap allocator over a region of memory it is given.
//!
//! Free blocks form a singly linked list sorted by address, with each block's header
//! stored in the free memory itself. Freed blocks are merged with free neighbours, so the
//! heap doesn't fragment into unusable slivers. Allocations carry no header: `dealloc`
//! works the size out again from the `Layout`.

use core::alloc::Layout;
use core::ptr::null_mut;

/// Every block is a multiple of this in size and aligned to it. It is also the smallest
/// block, since a free block has to hold its header.
const BLOCK_ALIGN: usize = 16;

struct FreeBlock {
    size: usize,
    next: *mut FreeBlock,
}

const _: () = assert!(size_of::<FreeBlock>() <= BLOCK_ALIGN);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    pub total: usize,
    pub used: usize,
    pub largest_free: usize,
}

/// Not thread-safe by itself; the kernel wraps it in a lock.
pub struct Heap {
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
    /// A heap with no memory; every allocation fails until `init`.
    pub const fn empty() -> Self {
        Heap { head: null_mut(), total: 0, used: 0 }
    }

    /// Takes over `start..end`.
    ///
    /// # Safety
    ///
    /// The range must be valid, writable memory that nothing else uses for as long as the
    /// heap does.
    pub unsafe fn init(&mut self, start: usize, end: usize) {
        let start = align_up(start, BLOCK_ALIGN);
        let end = end & !(BLOCK_ALIGN - 1);
        self.head = null_mut();
        self.total = end - start;
        self.used = 0;
        unsafe { self.free(start, end - start) };
    }

    /// Adds `start..end` to the heap, merging it with a free block that ends where it starts.
    /// For growing a heap as more memory becomes available.
    ///
    /// # Safety
    ///
    /// As for `init`, and the range must not overlap memory the heap already has.
    pub unsafe fn extend(&mut self, start: usize, end: usize) {
        let start = align_up(start, BLOCK_ALIGN);
        let end = end & !(BLOCK_ALIGN - 1);
        if end > start {
            self.total += end - start;
            unsafe { self.free(start, end - start) };
        }
    }

    /// Allocates a block for `layout`, or returns null if none is free.
    ///
    /// # Safety
    ///
    /// `init` must have been called.
    pub unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {
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

    /// Frees a block. Freeing memory that is already free panics.
    ///
    /// # Safety
    ///
    /// `ptr` must come from `alloc` on this heap with the same `layout`.
    pub unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) {
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

    pub fn stats(&self) -> Stats {
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    const SIZE: usize = 1 << 20;

    /// A heap over a leaked buffer whose start is deliberately misaligned.
    fn heap() -> (Heap, usize) {
        let region = vec![0u8; SIZE + 64].leak();
        let start = region.as_mut_ptr() as usize + 3;
        let mut heap = Heap::empty();
        unsafe { heap.init(start, start + SIZE) };
        (heap, start)
    }

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    #[test]
    fn starts_as_one_free_block() {
        let (heap, _) = heap();
        let stats = heap.stats();
        assert_eq!(stats.used, 0);
        assert_eq!(stats.largest_free, stats.total);
        assert!(stats.total > SIZE - 32);
    }

    #[test]
    fn extending_with_adjacent_memory_makes_one_bigger_block() {
        let region = vec![0u8; 3 * SIZE].leak();
        let start = region.as_mut_ptr() as usize;
        let mut heap = Heap::empty();
        unsafe { heap.init(start, start + SIZE) };
        let big = layout(SIZE + SIZE / 2, 16);
        assert!(unsafe { heap.alloc(big) }.is_null());

        unsafe { heap.extend(start + SIZE, start + 2 * SIZE) };
        assert_eq!(heap.stats().total, 2 * SIZE);
        assert_eq!(heap.stats().largest_free, 2 * SIZE);
        let block = unsafe { heap.alloc(big) };
        assert_eq!(block as usize, start);
        unsafe { heap.dealloc(block, big) };
        assert_eq!(heap.stats().used, 0);
    }

    #[test]
    fn respects_alignment_and_merges_freed_neighbours() {
        let (mut heap, _) = heap();
        let total = heap.stats().total;
        let a = unsafe { heap.alloc(layout(10, 1)) };
        let b = unsafe { heap.alloc(layout(100, 256)) };
        let c = unsafe { heap.alloc(layout(3000, 8)) };
        assert_eq!(b as usize % 256, 0);
        assert!(!a.is_null() && !c.is_null());
        unsafe {
            heap.dealloc(b, layout(100, 256));
            heap.dealloc(a, layout(10, 1));
            heap.dealloc(c, layout(3000, 8));
        }
        assert_eq!(heap.stats(), Stats { total, used: 0, largest_free: total });
    }

    #[test]
    fn returns_null_when_full() {
        let (mut heap, _) = heap();
        assert!(unsafe { heap.alloc(layout(SIZE * 2, 16)) }.is_null());
    }

    #[test]
    #[should_panic(expected = "already free")]
    fn double_free_panics() {
        let (mut heap, _) = heap();
        let l = layout(64, 16);
        let p = unsafe { heap.alloc(l) };
        unsafe {
            heap.dealloc(p, l);
            heap.dealloc(p, l);
        }
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// Random allocations and frees: blocks never overlap, stay aligned and keep their
    /// contents, and the heap is one block again once everything is freed.
    #[test]
    fn random_stress() {
        let (mut heap, start) = heap();
        let initial = heap.stats();
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let mut live: Vec<(usize, Layout, u8)> = Vec::new();

        for round in 0..50_000u32 {
            if live.is_empty() || rng.next() % 100 < 55 {
                let l = layout(1 + (rng.next() % 4096) as usize, 1 << (rng.next() % 9));
                let p = unsafe { heap.alloc(l) };
                if p.is_null() {
                    continue; // full
                }
                let p = p as usize;
                assert_eq!(p % l.align(), 0);
                assert!(p >= start && p + l.size() <= start + SIZE);
                for &(q, ql, _) in &live {
                    assert!(p + l.size() <= q || q + ql.size() <= p, "overlap in round {}", round);
                }
                let tag = (round % 251) as u8;
                unsafe { core::ptr::write_bytes(p as *mut u8, tag, l.size()) };
                live.push((p, l, tag));
            } else {
                let i = rng.next() as usize % live.len();
                let (p, l, tag) = live.swap_remove(i);
                let data = unsafe { core::slice::from_raw_parts(p as *const u8, l.size()) };
                assert!(data.iter().all(|&b| b == tag), "contents changed");
                unsafe { heap.dealloc(p as *mut u8, l) };
            }
        }
        for (p, l, _) in live {
            unsafe { heap.dealloc(p as *mut u8, l) };
        }
        assert_eq!(heap.stats(), initial);
    }
}
