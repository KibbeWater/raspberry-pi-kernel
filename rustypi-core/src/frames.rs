// frames.rs
//! Physical page frames: which 4KB pages of RAM are free, one bit each.
//!
//! Allocation is next-fit: the search resumes after the last frame handed out, so frames
//! are spread out rather than the same few reused, and a run of allocations is quick.

use alloc::vec;
use alloc::vec::Vec;
use rustypi_abi::layout::PAGE_SIZE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// Not the address of a frame this allocator manages.
    NotAFrame,
    /// Freed already, or never allocated.
    NotAllocated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameStats {
    pub total: usize,
    pub free: usize,
}

pub struct Frames {
    /// Physical address of frame 0.
    base: u64,
    count: usize,
    /// Bit `n` is set while frame `n` is allocated.
    used: Vec<u64>,
    free: usize,
    /// Where the next search starts, in words.
    next: usize,
}

impl Frames {
    /// Manages the whole pages in `start..end`, all free.
    pub fn new(start: u64, end: u64) -> Self {
        let base = start.next_multiple_of(PAGE_SIZE);
        let count = (end.saturating_sub(base) / PAGE_SIZE) as usize;
        let mut used = vec![0; count.div_ceil(64)];
        // Bits past the last frame count as allocated, so they're never handed out.
        if count % 64 != 0 {
            *used.last_mut().unwrap() = !0 << (count % 64);
        }
        Frames { base, count, used, free: count, next: 0 }
    }

    /// A free frame's physical address, now allocated. Its contents are whatever was there.
    pub fn allocate(&mut self) -> Option<u64> {
        let words = self.used.len();
        let word = (0..words).map(|i| (self.next + i) % words).find(|&w| self.used[w] != !0)?;
        let bit = self.used[word].trailing_ones() as usize;
        self.used[word] |= 1 << bit;
        self.free -= 1;
        self.next = word;
        Some(self.base + (word * 64 + bit) as u64 * PAGE_SIZE)
    }

    /// Makes an allocated frame free again.
    pub fn free(&mut self, frame: u64) -> Result<(), FrameError> {
        let offset = frame.checked_sub(self.base).ok_or(FrameError::NotAFrame)?;
        let n = (offset / PAGE_SIZE) as usize;
        if offset % PAGE_SIZE != 0 || n >= self.count {
            return Err(FrameError::NotAFrame);
        }
        let (word, bit) = (n / 64, n % 64);
        if self.used[word] & 1 << bit == 0 {
            return Err(FrameError::NotAllocated);
        }
        self.used[word] &= !(1 << bit);
        self.free += 1;
        Ok(())
    }

    pub fn stats(&self) -> FrameStats {
        FrameStats { total: self.count, free: self.free }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeSet;

    const BASE: u64 = 0x100_0000;

    #[test]
    fn only_whole_pages_inside_the_range_are_managed() {
        let frames = Frames::new(BASE + 1, BASE + 3 * PAGE_SIZE + 5);
        assert_eq!(frames.stats(), FrameStats { total: 2, free: 2 });
        assert_eq!(Frames::new(BASE, BASE).stats().total, 0);
        assert_eq!(Frames::new(BASE + PAGE_SIZE, BASE).stats().total, 0);
    }

    #[test]
    fn every_frame_is_handed_out_once_then_none() {
        let count = 130; // two full words and a partial one
        let mut frames = Frames::new(BASE, BASE + count * PAGE_SIZE);
        let got: BTreeSet<u64> = (0..count).map(|_| frames.allocate().unwrap()).collect();
        assert_eq!(got.len(), count as usize);
        assert!(got.iter().all(|&f| f % PAGE_SIZE == 0 && (BASE..BASE + count * PAGE_SIZE).contains(&f)));
        assert_eq!(frames.allocate(), None);
        assert_eq!(frames.stats().free, 0);
    }

    #[test]
    fn freed_frames_come_back() {
        let mut frames = Frames::new(BASE, BASE + 4 * PAGE_SIZE);
        let all: Vec<u64> = (0..4).map(|_| frames.allocate().unwrap()).collect();
        frames.free(all[2]).unwrap();
        assert_eq!(frames.stats().free, 1);
        assert_eq!(frames.allocate(), Some(all[2]));
    }

    #[test]
    fn bad_frees_are_errors() {
        let mut frames = Frames::new(BASE, BASE + 4 * PAGE_SIZE);
        let frame = frames.allocate().unwrap();
        assert_eq!(frames.free(frame + 8), Err(FrameError::NotAFrame));
        assert_eq!(frames.free(BASE - PAGE_SIZE), Err(FrameError::NotAFrame));
        assert_eq!(frames.free(BASE + 4 * PAGE_SIZE), Err(FrameError::NotAFrame));
        assert_eq!(frames.free(BASE + PAGE_SIZE), Err(FrameError::NotAllocated));
        frames.free(frame).unwrap();
        assert_eq!(frames.free(frame), Err(FrameError::NotAllocated));
        assert_eq!(frames.stats().free, 4);
    }
}
