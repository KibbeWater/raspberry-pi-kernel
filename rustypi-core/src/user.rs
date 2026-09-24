// user.rs
//! Checks on memory that user programs hand to the kernel.

/// A range of user addresses, `start..start + len`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub start: u64,
    pub len: u64,
}

impl Region {
    pub const fn new(start: u64, len: u64) -> Self {
        Region { start, len }
    }

    /// Whether `addr..addr + len` lies entirely inside this region. A range that wraps past
    /// the end of the address space never does.
    pub fn contains(&self, addr: u64, len: u64) -> bool {
        let Some(end) = addr.checked_add(len) else { return false };
        addr >= self.start && end <= self.start + self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Region = Region::new(0x8000_0000, 0x20_0000);

    #[test]
    fn ranges_inside_the_region_are_accepted() {
        assert!(WINDOW.contains(0x8000_0000, 16));
        assert!(WINDOW.contains(0x8000_0000, 0x20_0000));
        assert!(WINDOW.contains(0x801F_FFF0, 16));
        assert!(WINDOW.contains(0x8020_0000, 0)); // empty, at the very end
    }

    #[test]
    fn ranges_leaving_the_region_are_rejected() {
        assert!(!WINDOW.contains(0x80000, 4)); // kernel memory
        assert!(!WINDOW.contains(0x7FFF_FFFF, 2)); // starts just before
        assert!(!WINDOW.contains(0x801F_FFFE, 4)); // runs off the end
        assert!(!WINDOW.contains(0x8000_0010, u64::MAX)); // wraps around
    }
}
