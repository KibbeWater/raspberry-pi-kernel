// paging.rs
//! Page tables for user address spaces.
//!
//! Every program gets a level 1 table of its own. It shares the kernel's entries, so the
//! kernel stays mapped while it handles the program's exceptions (EL0 still can't touch it),
//! and adds one entry for the user window, the 1GB at `USER_BASE` (`USER_BASE..USER_END`
//! must be exactly one level 1 entry), mapped with 4KB pages
//! through level 2 and level 3 tables that belong to the program.
//!
//! Pages are readable from EL0 and at most one of writable and executable: `Access` has no
//! way to ask for both. They are not global, so the TLB tags them with the program's ASID
//! and a task switch doesn't need to flush it.
//!
//! Tables and pages are page frames from a `FrameSource`, and hold physical addresses. The
//! kernel identity-maps RAM, so a physical address is also a pointer the kernel can follow;
//! host tests use heap pointers the same way.

use alloc::collections::BTreeMap;

pub use rustypi_abi::layout::{USER_BASE, USER_END};

pub const PAGE_SIZE: usize = rustypi_abi::layout::PAGE_SIZE as usize;

const ENTRIES: usize = 512;
const PAGE: u64 = PAGE_SIZE as u64;

/// One translation table, in the layout the MMU walks.
#[repr(C, align(4096))]
pub struct Table(pub [u64; ENTRIES]);

/// Where address spaces get page frames (for their pages and tables), and give them back.
pub trait FrameSource: Sync {
    /// The physical address of a free frame, or `None` if memory ran out. Its contents are
    /// unspecified.
    fn allocate(&self) -> Option<u64>;

    /// Gives back a frame from `allocate`.
    ///
    /// # Safety
    /// Nothing may use the frame any more.
    unsafe fn free(&self, frame: u64);
}

/// There is no frame left for a page or table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutOfMemory;

/// A zeroed frame from `source`.
fn zeroed_frame(source: &dyn FrameSource) -> Result<u64, OutOfMemory> {
    let frame = source.allocate().ok_or(OutOfMemory)?;
    unsafe { core::ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE) };
    Ok(frame)
}

/// The table in the frame at `physical`.
///
/// # Safety
/// It must be one of this address space's tables, and not otherwise borrowed.
unsafe fn table<'a>(physical: u64) -> &'a mut Table {
    unsafe { &mut *(physical as *mut Table) }
}

// Descriptor bits (VMSAv8-64, 4KB granule).
const VALID: u64 = 1 << 0;
/// At levels 1 and 2: points to a table rather than being a block. At level 3: required.
const TABLE_OR_PAGE: u64 = 1 << 1;
/// MAIR index 0, which the kernel sets to Normal write-back memory.
const ATTR_NORMAL: u64 = 0 << 2;
/// AP[1]: EL0 may access it.
const AP_EL0: u64 = 1 << 6;
/// AP[2]: nobody may write it.
const AP_READ_ONLY: u64 = 1 << 7;
const INNER_SHAREABLE: u64 = 3 << 8;
const ACCESS_FLAG: u64 = 1 << 10;
const NOT_GLOBAL: u64 = 1 << 11;
const PRIVILEGED_EXECUTE_NEVER: u64 = 1 << 53;
const USER_EXECUTE_NEVER: u64 = 1 << 54;
const ADDRESS: u64 = 0x0000_FFFF_FFFF_F000;

/// What a program may do with a page. It can always read it; the kernel never executes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    ReadOnly,
    ReadWrite,
    ReadExecute,
}

impl Access {
    const ALL: [Access; 3] = [Access::ReadOnly, Access::ReadWrite, Access::ReadExecute];

    const fn descriptor_bits(self) -> u64 {
        let common = VALID
            | TABLE_OR_PAGE
            | ATTR_NORMAL
            | AP_EL0
            | INNER_SHAREABLE
            | ACCESS_FLAG
            | NOT_GLOBAL
            | PRIVILEGED_EXECUTE_NEVER;
        match self {
            Access::ReadOnly => common | AP_READ_ONLY | USER_EXECUTE_NEVER,
            Access::ReadWrite => common | USER_EXECUTE_NEVER,
            Access::ReadExecute => common | AP_READ_ONLY,
        }
    }

    fn from_descriptor(descriptor: u64) -> Option<Access> {
        Access::ALL.into_iter().find(|access| access.descriptor_bits() == descriptor & !ADDRESS)
    }

    pub fn writable(self) -> bool {
        self == Access::ReadWrite
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    /// The address isn't the start of a page.
    NotAligned,
    /// The page lies outside the user window.
    OutsideWindow,
    /// Something is mapped there already.
    AlreadyMapped,
    /// There is no frame left for the page or a table.
    OutOfMemory,
}

impl From<OutOfMemory> for MapError {
    fn from(_: OutOfMemory) -> Self {
        MapError::OutOfMemory
    }
}

/// A program may not access `address` that way: it is unmapped, or not writable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessFault {
    pub address: u64,
}

/// A program's memory: its page tables and the frames they map. Dropping it frees them all,
/// so the kernel must stop using it first (switch TTBR0 away and flush the TLB).
pub struct AddressSpace {
    source: &'static dyn FrameSource,
    /// Physical addresses of its tables.
    root: u64,
    level2: u64,
    /// By level 2 index.
    level3: BTreeMap<usize, u64>,
    /// Physical addresses of its pages, by virtual address.
    frames: BTreeMap<u64, u64>,
}

fn level1_index(va: u64) -> usize {
    (va >> 30) as usize % ENTRIES
}

fn level2_index(va: u64) -> usize {
    (va >> 21) as usize % ENTRIES
}

fn level3_index(va: u64) -> usize {
    (va >> 12) as usize % ENTRIES
}

impl AddressSpace {
    /// An address space with the kernel's entries from `kernel`, its level 1 table, and
    /// nothing mapped in the user window. Its frames come from `source`.
    pub fn new(kernel: &Table, source: &'static dyn FrameSource) -> Result<Self, OutOfMemory> {
        let root = zeroed_frame(source)?;
        let Ok(level2) = zeroed_frame(source) else {
            unsafe { source.free(root) };
            return Err(OutOfMemory);
        };
        let root_table = unsafe { table(root) };
        root_table.0 = kernel.0;
        root_table.0[level1_index(USER_BASE)] = level2 | VALID | TABLE_OR_PAGE;
        Ok(AddressSpace { source, root, level2, level3: BTreeMap::new(), frames: BTreeMap::new() })
    }

    /// The TTBR0 value that makes this the current address space, tagged with `asid`.
    pub fn translation_base(&self, asid: u8) -> u64 {
        self.root | (asid as u64) << 48
    }

    /// Maps a zeroed page at `va`.
    pub fn map(&mut self, va: u64, access: Access) -> Result<(), MapError> {
        if va % PAGE != 0 {
            return Err(MapError::NotAligned);
        }
        if !(USER_BASE..USER_END).contains(&va) {
            return Err(MapError::OutsideWindow);
        }
        if self.frames.contains_key(&va) {
            return Err(MapError::AlreadyMapped);
        }
        let level3 = match self.level3.get(&level2_index(va)) {
            Some(&level3) => level3,
            None => {
                let level3 = zeroed_frame(self.source)?;
                unsafe { table(self.level2) }.0[level2_index(va)] = level3 | VALID | TABLE_OR_PAGE;
                self.level3.insert(level2_index(va), level3);
                level3
            }
        };
        let frame = zeroed_frame(self.source)?;
        unsafe { table(level3) }.0[level3_index(va)] = frame | access.descriptor_bits();
        self.frames.insert(va, frame);
        Ok(())
    }

    /// Maps zeroed pages over `start..start + len`, from a page boundary.
    pub fn map_range(&mut self, start: u64, len: u64, access: Access) -> Result<(), MapError> {
        let end = start.checked_add(len).ok_or(MapError::OutsideWindow)?;
        (start..end).step_by(PAGE_SIZE).try_for_each(|va| self.map(va, access))
    }

    /// The level 3 descriptor for `va`, found by walking the tables like the MMU does.
    fn descriptor(&self, va: u64) -> Option<u64> {
        fn next(descriptor: u64) -> Option<&'static Table> {
            (descriptor & (VALID | TABLE_OR_PAGE) == VALID | TABLE_OR_PAGE)
                .then(|| unsafe { &*((descriptor & ADDRESS) as *const Table) })
        }
        if !(USER_BASE..USER_END).contains(&va) {
            return None;
        }
        let level2 = next(unsafe { table(self.root) }.0[level1_index(va)])?;
        let level3 = next(level2.0[level2_index(va)])?;
        let descriptor = level3.0[level3_index(va)];
        (descriptor & VALID != 0).then_some(descriptor)
    }

    /// Where `va` is in physical memory, and how the program may use it.
    pub fn translate(&self, va: u64) -> Option<(u64, Access)> {
        let descriptor = self.descriptor(va)?;
        Some((descriptor & ADDRESS | va % PAGE, Access::from_descriptor(descriptor)?))
    }

    /// Calls `f` with each page-sized piece of `va..va + len`: its physical address, its
    /// offset in the range, and its length. Stops at the first piece that `allowed` refuses.
    fn for_each_piece(
        &self,
        va: u64,
        len: usize,
        allowed: impl Fn(Access) -> bool,
        mut f: impl FnMut(u64, usize, usize),
    ) -> Result<(), AccessFault> {
        let mut done = 0;
        while done < len {
            let address = va.checked_add(done as u64).ok_or(AccessFault { address: u64::MAX })?;
            let piece = (PAGE_SIZE - (address % PAGE) as usize).min(len - done);
            match self.translate(address) {
                Some((physical, access)) if allowed(access) => f(physical, done, piece),
                _ => return Err(AccessFault { address }),
            }
            done += piece;
        }
        Ok(())
    }

    /// Copies `bytes` into mapped pages at `va`, whatever the program may do with them. For
    /// setting a program up, like loading its code.
    pub fn load(&mut self, va: u64, bytes: &[u8]) -> Result<(), AccessFault> {
        self.for_each_piece(va, bytes.len(), |_| true, |physical, offset, len| unsafe {
            core::ptr::copy_nonoverlapping(bytes[offset..].as_ptr(), physical as *mut u8, len);
        })
    }

    /// Copies program memory at `va` into `buf`, if the program could read all of it.
    pub fn read_user(&self, va: u64, buf: &mut [u8]) -> Result<(), AccessFault> {
        let dst = buf.as_mut_ptr();
        self.for_each_piece(va, buf.len(), |_| true, |physical, offset, len| unsafe {
            core::ptr::copy_nonoverlapping(physical as *const u8, dst.add(offset), len);
        })
    }

    /// Copies `bytes` into program memory at `va`, if the program could write all of it.
    pub fn write_user(&mut self, va: u64, bytes: &[u8]) -> Result<(), AccessFault> {
        self.for_each_piece(va, bytes.len(), Access::writable, |physical, offset, len| unsafe {
            core::ptr::copy_nonoverlapping(bytes[offset..].as_ptr(), physical as *mut u8, len);
        })
    }

    /// Mapped pages.
    pub fn pages(&self) -> usize {
        self.frames.len()
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        let tables = [self.root, self.level2].into_iter().chain(self.level3.values().copied());
        for frame in self.frames.values().copied().chain(tables) {
            unsafe { self.source.free(frame) };
        }
    }
}

/// Frames for host tests, from the heap.
#[cfg(test)]
pub(crate) mod testing {
    use super::{FrameSource, PAGE_SIZE};
    use alloc::alloc::{alloc, dealloc, Layout};
    use alloc::boxed::Box;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// Hands out at most `limit` frames at a time, filled with junk so tests notice missing
    /// zeroing, and counts how many are out so they notice leaks.
    pub struct TestFrames {
        live: AtomicUsize,
        limit: usize,
    }

    const LAYOUT: Layout = match Layout::from_size_align(PAGE_SIZE, PAGE_SIZE) {
        Ok(layout) => layout,
        Err(_) => panic!(),
    };

    impl TestFrames {
        pub fn new(limit: usize) -> &'static TestFrames {
            Box::leak(Box::new(TestFrames { live: AtomicUsize::new(0), limit }))
        }

        pub fn unlimited() -> &'static TestFrames {
            TestFrames::new(usize::MAX)
        }

        pub fn live(&self) -> usize {
            self.live.load(Ordering::Relaxed)
        }
    }

    impl FrameSource for TestFrames {
        fn allocate(&self) -> Option<u64> {
            if self.live() >= self.limit {
                return None;
            }
            self.live.fetch_add(1, Ordering::Relaxed);
            let frame = unsafe { alloc(LAYOUT) };
            assert!(!frame.is_null());
            unsafe { core::ptr::write_bytes(frame, 0xAA, PAGE_SIZE) };
            Some(frame as u64)
        }

        unsafe fn free(&self, frame: u64) {
            self.live.fetch_sub(1, Ordering::Relaxed);
            unsafe { dealloc(frame as *mut u8, LAYOUT) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::TestFrames;
    use super::*;
    use alloc::boxed::Box;
    use alloc::vec;
    use alloc::vec::Vec;

    const KERNEL_RAM: u64 = 0x1234_0000 | VALID | TABLE_OR_PAGE;
    const KERNEL_DEVICES: u64 = 0x4000_0000 | VALID;

    fn kernel() -> Box<Table> {
        let mut kernel = Box::new(Table([0; ENTRIES]));
        kernel.0[0] = KERNEL_RAM;
        kernel.0[1] = KERNEL_DEVICES;
        kernel
    }

    fn space() -> AddressSpace {
        AddressSpace::new(&kernel(), TestFrames::unlimited()).unwrap()
    }

    fn root(space: &AddressSpace) -> &Table {
        unsafe { table(space.root) }
    }

    #[test]
    fn new_spaces_share_the_kernel_entries_and_map_nothing() {
        let space = space();
        assert_eq!(root(&space).0[0], KERNEL_RAM);
        assert_eq!(root(&space).0[1], KERNEL_DEVICES);
        assert_eq!(root(&space).0[2] & (VALID | TABLE_OR_PAGE), VALID | TABLE_OR_PAGE);
        assert_eq!(space.translate(USER_BASE), None);
        assert_eq!(space.pages(), 0);
    }

    #[test]
    fn mapped_pages_translate_to_distinct_frames_with_their_access() {
        let mut space = space();
        space.map(USER_BASE, Access::ReadExecute).unwrap();
        space.map(USER_BASE + PAGE, Access::ReadOnly).unwrap();
        // Last page of the window, which needs a level 3 table of its own.
        space.map(USER_END - PAGE, Access::ReadWrite).unwrap();

        let (code, access) = space.translate(USER_BASE + 0x10).unwrap();
        assert_eq!((code % PAGE, access), (0x10, Access::ReadExecute));
        let (data, access) = space.translate(USER_BASE + PAGE).unwrap();
        assert_eq!(access, Access::ReadOnly);
        let (stack, access) = space.translate(USER_END - 1).unwrap();
        assert_eq!((stack % PAGE, access), (PAGE - 1, Access::ReadWrite));
        assert!(code - 0x10 != data && data != stack - (PAGE - 1));
        assert_eq!(space.level3.len(), 2);
        assert_eq!(space.translate(USER_BASE + 2 * PAGE), None);
    }

    #[test]
    fn descriptors_carry_the_permissions() {
        let mut space = space();
        for (i, access) in Access::ALL.into_iter().enumerate() {
            space.map(USER_BASE + i as u64 * PAGE, access).unwrap();
        }
        let bits = |i: u64| space.descriptor(USER_BASE + i * PAGE).unwrap() & !ADDRESS;
        for i in 0..3 {
            // Every user page: EL0 may access it, the kernel may not execute it, and it is
            // tagged with the ASID.
            let required = AP_EL0 | PRIVILEGED_EXECUTE_NEVER | NOT_GLOBAL | ACCESS_FLAG | VALID | TABLE_OR_PAGE;
            assert_eq!(bits(i) & required, required);
        }
        let (read_only, read_write, read_execute) = (bits(0), bits(1), bits(2));
        assert!(read_only & AP_READ_ONLY != 0 && read_only & USER_EXECUTE_NEVER != 0);
        assert!(read_write & AP_READ_ONLY == 0 && read_write & USER_EXECUTE_NEVER != 0);
        assert!(read_execute & AP_READ_ONLY != 0 && read_execute & USER_EXECUTE_NEVER == 0);
    }

    #[test]
    fn bad_mappings_are_refused() {
        let mut space = space();
        assert_eq!(space.map(USER_BASE + 1, Access::ReadOnly), Err(MapError::NotAligned));
        assert_eq!(space.map(0x80000, Access::ReadOnly), Err(MapError::OutsideWindow));
        assert_eq!(space.map(USER_END, Access::ReadOnly), Err(MapError::OutsideWindow));
        space.map(USER_BASE, Access::ReadOnly).unwrap();
        assert_eq!(space.map(USER_BASE, Access::ReadWrite), Err(MapError::AlreadyMapped));
        assert_eq!(space.map_range(USER_END - PAGE, 2 * PAGE, Access::ReadOnly), Err(MapError::OutsideWindow));
    }

    #[test]
    fn ranges_cover_partial_pages() {
        let mut space = space();
        space.map_range(USER_BASE, PAGE + 1, Access::ReadWrite).unwrap();
        assert_eq!(space.pages(), 2);
        assert!(space.translate(USER_BASE + 2 * PAGE).is_none());
    }

    #[test]
    fn copies_cross_page_boundaries() {
        let mut space = space();
        space.map_range(USER_BASE, 2 * PAGE, Access::ReadWrite).unwrap();
        let data: Vec<u8> = (0..200).collect();
        let at = USER_BASE + PAGE - 100;
        space.write_user(at, &data).unwrap();
        let mut back = vec![0; 200];
        space.read_user(at, &mut back).unwrap();
        assert_eq!(back, data);
        // And the bytes really are split over the two frames.
        let (second, _) = space.translate(USER_BASE + PAGE).unwrap();
        assert_eq!(unsafe { *(second as *const u8) }, 100);
    }

    #[test]
    fn user_copies_stop_at_unmapped_and_read_only_pages() {
        let mut space = space();
        space.map(USER_BASE, Access::ReadExecute).unwrap();
        space.map(USER_BASE + PAGE, Access::ReadWrite).unwrap();
        let mut buf = [0; 8];
        // Running off the end of the mapping into the unmapped third page.
        let fault = space.read_user(USER_BASE + 2 * PAGE - 4, &mut buf);
        assert_eq!(fault, Err(AccessFault { address: USER_BASE + 2 * PAGE }));
        assert_eq!(space.read_user(0x80000, &mut buf), Err(AccessFault { address: 0x80000 }));
        // Code is readable, but only the kernel may write it.
        assert!(space.read_user(USER_BASE, &mut buf).is_ok());
        assert_eq!(space.write_user(USER_BASE, &buf), Err(AccessFault { address: USER_BASE }));
        assert!(space.load(USER_BASE, b"code").is_ok());
        // Empty copies touch nothing.
        assert!(space.read_user(0, &mut []).is_ok());
    }

    #[test]
    fn copies_that_wrap_the_address_space_fault() {
        let space = space();
        let mut buf = [0; 16];
        assert!(space.read_user(u64::MAX - 4, &mut buf).is_err());
    }

    #[test]
    fn spaces_are_independent() {
        let mut a = space();
        let mut b = space();
        a.map(USER_BASE, Access::ReadWrite).unwrap();
        b.map(USER_BASE, Access::ReadWrite).unwrap();
        a.write_user(USER_BASE, b"aaaa").unwrap();
        b.write_user(USER_BASE, b"bbbb").unwrap();
        let mut buf = [0; 4];
        a.read_user(USER_BASE, &mut buf).unwrap();
        assert_eq!(&buf, b"aaaa");
        assert_ne!(a.translation_base(1) & ADDRESS, b.translation_base(1) & ADDRESS);
    }

    #[test]
    fn the_translation_base_carries_the_asid() {
        let space = space();
        let base = space.translation_base(0xAB);
        assert_eq!(base >> 48, 0xAB);
        assert_eq!(base & ADDRESS, space.root);
    }

    #[test]
    fn new_pages_are_zeroed() {
        let mut space = space();
        space.map(USER_BASE, Access::ReadWrite).unwrap();
        let mut buf = [0xFF; PAGE_SIZE];
        space.read_user(USER_BASE, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn dropping_a_space_gives_back_every_frame() {
        let frames = TestFrames::unlimited();
        let mut space = AddressSpace::new(&kernel(), frames).unwrap();
        space.map_range(USER_BASE, 3 * PAGE, Access::ReadExecute).unwrap();
        space.map(USER_END - PAGE, Access::ReadWrite).unwrap();
        // 4 pages, the root, level 2, and two level 3 tables.
        assert_eq!(frames.live(), 8);
        drop(space);
        assert_eq!(frames.live(), 0);
    }

    #[test]
    fn running_out_of_frames_is_an_error_and_leaks_nothing() {
        let frames = TestFrames::new(1);
        assert!(AddressSpace::new(&kernel(), frames).is_err());
        assert_eq!(frames.live(), 0);

        // The root, level 2, a level 3 table and two pages.
        let frames = TestFrames::new(5);
        let mut space = AddressSpace::new(&kernel(), frames).unwrap();
        assert_eq!(space.map_range(USER_BASE, 3 * PAGE, Access::ReadWrite), Err(MapError::OutOfMemory));
        assert_eq!(space.pages(), 2);
        drop(space);
        assert_eq!(frames.live(), 0);
    }
}
