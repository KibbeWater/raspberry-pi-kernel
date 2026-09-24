// elf.rs
//! Loads user programs: statically linked AArch64 ELF64 executables.
//!
//! Only what a program needs to run is read: the file header and the `PT_LOAD` program
//! headers. Every segment must start on a page boundary (the `rustypi-user` linker script
//! arranges that), lie between `USER_BASE` and `PROGRAM_END`, and be writable or executable
//! but not both. Anything else is refused before a single page is mapped.

use alloc::vec::Vec;
use rustypi_abi::layout::{PAGE_SIZE, PROGRAM_END, USER_BASE};
use crate::paging::{Access, AccessFault, AddressSpace, MapError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfError {
    /// Shorter than its headers say it is.
    Truncated,
    /// No ELF magic number.
    NotElf,
    /// Not a little-endian, 64-bit, AArch64 file.
    WrongArchitecture,
    /// Not a statically linked executable (a shared library or position-independent one).
    NotExecutable,
    /// Has no loadable segments.
    NoSegments,
    /// A segment doesn't start on a page boundary.
    Unaligned,
    /// A segment reaches outside `USER_BASE..PROGRAM_END`.
    OutsideProgramArea,
    /// A segment is bigger in the file than in memory.
    BadSize,
    /// A segment is both writable and executable.
    WritableAndExecutable,
    /// Two segments share a page.
    Overlapping,
    /// The entry point isn't in an executable segment.
    BadEntry,
}

impl ElfError {
    pub fn description(self) -> &'static str {
        match self {
            ElfError::Truncated => "file is truncated",
            ElfError::NotElf => "not an ELF file",
            ElfError::WrongArchitecture => "not a 64-bit little-endian AArch64 program",
            ElfError::NotExecutable => "not a statically linked executable",
            ElfError::NoSegments => "nothing to load",
            ElfError::Unaligned => "segment not page aligned",
            ElfError::OutsideProgramArea => "segment outside the program area",
            ElfError::BadSize => "segment bigger in the file than in memory",
            ElfError::WritableAndExecutable => "segment both writable and executable",
            ElfError::Overlapping => "segments overlap",
            ElfError::BadEntry => "entry point not in executable code",
        }
    }
}

/// A validated program, borrowing its bytes from the file.
#[derive(Debug, PartialEq, Eq)]
pub struct Program<'a> {
    pub entry: u64,
    /// In address order.
    pub segments: Vec<Segment<'a>>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Segment<'a> {
    pub address: u64,
    /// Bytes in memory. The part past `data` is zero (`.bss`).
    pub size: u64,
    pub data: &'a [u8],
    pub access: Access,
}

impl Segment<'_> {
    pub fn end(&self) -> u64 {
        self.address + self.size
    }
}

const MAGIC: &[u8; 4] = b"\x7fELF";
const CLASS_64: u8 = 2;
const LITTLE_ENDIAN: u8 = 1;
const TYPE_EXECUTABLE: u16 = 2;
const MACHINE_AARCH64: u16 = 183;
const HEADER_SIZE: usize = 64;
const PROGRAM_HEADER_SIZE: usize = 56;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, ElfError> {
    let field = bytes.get(at..at + 2).ok_or(ElfError::Truncated)?;
    Ok(u16::from_le_bytes(field.try_into().unwrap()))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, ElfError> {
    let field = bytes.get(at..at + 4).ok_or(ElfError::Truncated)?;
    Ok(u32::from_le_bytes(field.try_into().unwrap()))
}

fn u64_at(bytes: &[u8], at: usize) -> Result<u64, ElfError> {
    let field = bytes.get(at..at + 8).ok_or(ElfError::Truncated)?;
    Ok(u64::from_le_bytes(field.try_into().unwrap()))
}

/// Checks `bytes` is a program RustyPI can run, and finds its segments.
pub fn parse(bytes: &[u8]) -> Result<Program<'_>, ElfError> {
    if bytes.len() < HEADER_SIZE {
        return Err(if bytes.starts_with(MAGIC) { ElfError::Truncated } else { ElfError::NotElf });
    }
    if &bytes[..4] != MAGIC {
        return Err(ElfError::NotElf);
    }
    if bytes[4] != CLASS_64 || bytes[5] != LITTLE_ENDIAN || u16_at(bytes, 18)? != MACHINE_AARCH64 {
        return Err(ElfError::WrongArchitecture);
    }
    if u16_at(bytes, 16)? != TYPE_EXECUTABLE {
        return Err(ElfError::NotExecutable);
    }
    let entry = u64_at(bytes, 24)?;
    let table = u64_at(bytes, 32)? as usize;
    let entry_size = u16_at(bytes, 54)? as usize;
    let count = u16_at(bytes, 56)? as usize;
    if count > 0 && entry_size < PROGRAM_HEADER_SIZE {
        return Err(ElfError::Truncated);
    }

    let mut segments = Vec::new();
    for i in 0..count {
        let header = table.checked_add(i * entry_size).ok_or(ElfError::Truncated)?;
        if u32_at(bytes, header)? != PT_LOAD {
            continue;
        }
        let flags = u32_at(bytes, header + 4)?;
        let offset = u64_at(bytes, header + 8)? as usize;
        let address = u64_at(bytes, header + 16)?;
        let file_size = u64_at(bytes, header + 32)? as usize;
        let size = u64_at(bytes, header + 40)?;

        if address % PAGE_SIZE != 0 {
            return Err(ElfError::Unaligned);
        }
        let end = address.checked_add(size).ok_or(ElfError::OutsideProgramArea)?;
        if address < USER_BASE || end > PROGRAM_END {
            return Err(ElfError::OutsideProgramArea);
        }
        if file_size as u64 > size {
            return Err(ElfError::BadSize);
        }
        let data = offset
            .checked_add(file_size)
            .and_then(|data_end| bytes.get(offset..data_end))
            .ok_or(ElfError::Truncated)?;
        let access = match (flags & PF_W != 0, flags & PF_X != 0) {
            (true, true) => return Err(ElfError::WritableAndExecutable),
            (true, false) => Access::ReadWrite,
            (false, true) => Access::ReadExecute,
            (false, false) => Access::ReadOnly,
        };
        if size > 0 {
            segments.push(Segment { address, size, data, access });
        }
    }

    if segments.is_empty() {
        return Err(ElfError::NoSegments);
    }
    segments.sort_by_key(|segment| segment.address);
    for pair in segments.windows(2) {
        if pair[0].end().next_multiple_of(PAGE_SIZE) > pair[1].address {
            return Err(ElfError::Overlapping);
        }
    }
    let runs_entry = |segment: &Segment| {
        segment.access == Access::ReadExecute && (segment.address..segment.end()).contains(&entry)
    };
    if !segments.iter().any(runs_entry) {
        return Err(ElfError::BadEntry);
    }
    Ok(Program { entry, segments })
}

impl Program<'_> {
    /// Maps and fills every segment in `space`.
    pub fn load(&self, space: &mut AddressSpace) -> Result<(), LoadError> {
        for segment in &self.segments {
            space.map_range(segment.address, segment.size, segment.access).map_err(LoadError::Map)?;
            space.load(segment.address, segment.data).map_err(LoadError::Access)?;
        }
        Ok(())
    }
}

/// Loading a valid program failed: the address space already had something in its way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadError {
    Map(MapError),
    Access(AccessFault),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paging::testing::TestFrames;
    use crate::paging::Table;
    use alloc::boxed::Box;
    use alloc::vec;

    /// A test segment: flags, address, file bytes and size in memory.
    struct Spec {
        flags: u32,
        address: u64,
        data: Vec<u8>,
        size: u64,
    }

    fn spec(flags: u32, address: u64, data: &[u8], size: u64) -> Spec {
        Spec { flags, address, data: data.to_vec(), size }
    }

    const R: u32 = 4;
    const RX: u32 = 4 | PF_X;
    const RW: u32 = 4 | PF_W;

    /// Builds an ELF file with one program header per segment, data after the headers.
    fn build(entry: u64, segments: &[Spec]) -> Vec<u8> {
        let table = HEADER_SIZE;
        let mut data_at = table + segments.len() * PROGRAM_HEADER_SIZE;
        let mut out = vec![0; data_at];
        out[..4].copy_from_slice(MAGIC);
        out[4] = CLASS_64;
        out[5] = LITTLE_ENDIAN;
        out[6] = 1; // version
        out[16..18].copy_from_slice(&TYPE_EXECUTABLE.to_le_bytes());
        out[18..20].copy_from_slice(&MACHINE_AARCH64.to_le_bytes());
        out[24..32].copy_from_slice(&entry.to_le_bytes());
        out[32..40].copy_from_slice(&(table as u64).to_le_bytes());
        out[52..54].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
        out[54..56].copy_from_slice(&(PROGRAM_HEADER_SIZE as u16).to_le_bytes());
        out[56..58].copy_from_slice(&(segments.len() as u16).to_le_bytes());
        for (i, segment) in segments.iter().enumerate() {
            let header = table + i * PROGRAM_HEADER_SIZE;
            out[header..header + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
            out[header + 4..header + 8].copy_from_slice(&segment.flags.to_le_bytes());
            out[header + 8..header + 16].copy_from_slice(&(data_at as u64).to_le_bytes());
            out[header + 16..header + 24].copy_from_slice(&segment.address.to_le_bytes());
            out[header + 24..header + 32].copy_from_slice(&segment.address.to_le_bytes());
            out[header + 32..header + 40].copy_from_slice(&(segment.data.len() as u64).to_le_bytes());
            out[header + 40..header + 48].copy_from_slice(&segment.size.to_le_bytes());
            data_at += segment.data.len();
        }
        for segment in segments {
            out.extend_from_slice(&segment.data);
        }
        out
    }

    const CODE: u64 = USER_BASE;
    const DATA: u64 = USER_BASE + 0x1000;

    /// Code, then data with a `.bss` tail.
    fn typical() -> Vec<u8> {
        build(CODE + 4, &[spec(RX, CODE, b"code....", 8), spec(RW, DATA, b"data", 0x1800)])
    }

    #[test]
    fn a_typical_program_parses() {
        let file = typical();
        let program = parse(&file).unwrap();
        assert_eq!(program.entry, CODE + 4);
        assert_eq!(program.segments.len(), 2);
        assert_eq!(program.segments[0].access, Access::ReadExecute);
        assert_eq!(program.segments[0].data, b"code....");
        assert_eq!(program.segments[1].access, Access::ReadWrite);
        assert_eq!(program.segments[1].size, 0x1800);
    }

    #[test]
    fn loading_maps_every_segment_and_zeroes_bss() {
        let file = typical();
        let program = parse(&file).unwrap();
        let kernel = Box::new(Table([0; 512]));
        let mut space = AddressSpace::new(&kernel, TestFrames::unlimited()).unwrap();
        program.load(&mut space).unwrap();

        assert_eq!(space.pages(), 3); // one code page, two data pages
        let mut buf = [0xFF; 8];
        space.read_user(CODE, &mut buf).unwrap();
        assert_eq!(&buf, b"code....");
        space.read_user(DATA + 0x17F8, &mut buf).unwrap();
        assert_eq!(buf, [0; 8]);
        assert_eq!(space.translate(CODE).unwrap().1, Access::ReadExecute);
        assert_eq!(space.translate(DATA + 0x1000).unwrap().1, Access::ReadWrite);
        // Loading it again finds its own pages in the way.
        assert!(matches!(program.load(&mut space), Err(LoadError::Map(MapError::AlreadyMapped))));
    }

    #[test]
    fn segments_come_out_in_address_order() {
        let file = build(CODE, &[spec(R, DATA, b"ro", 2), spec(RX, CODE, b"x", 1)]);
        let program = parse(&file).unwrap();
        assert_eq!(program.segments[0].address, CODE);
        assert_eq!(program.segments[1].access, Access::ReadOnly);
    }

    #[test]
    fn non_programs_are_refused() {
        assert_eq!(parse(b"hello"), Err(ElfError::NotElf));
        assert_eq!(parse(b"\x7fELF"), Err(ElfError::Truncated));
        let mut file = typical();
        file[0] = b'X';
        assert_eq!(parse(&file), Err(ElfError::NotElf));
    }

    #[test]
    fn other_architectures_and_file_types_are_refused() {
        let patched = |at: usize, bytes: &[u8]| {
            let mut file = typical();
            file[at..at + bytes.len()].copy_from_slice(bytes);
            parse(&file).map(|_| ())
        };
        assert_eq!(patched(4, &[1]), Err(ElfError::WrongArchitecture)); // 32-bit
        assert_eq!(patched(5, &[2]), Err(ElfError::WrongArchitecture)); // big endian
        assert_eq!(patched(18, &62u16.to_le_bytes()), Err(ElfError::WrongArchitecture)); // x86-64
        assert_eq!(patched(16, &3u16.to_le_bytes()), Err(ElfError::NotExecutable)); // PIE / .so
    }

    #[test]
    fn segments_must_be_aligned_and_inside_the_program_area() {
        let one = |segment: Spec| parse(&build(segment.address, &[segment])).map(|_| ());
        assert_eq!(one(spec(RX, CODE + 4, b"x", 1)), Err(ElfError::Unaligned));
        assert_eq!(one(spec(RX, 0x80000, b"x", 1)), Err(ElfError::OutsideProgramArea));
        // Running into the guard page below the stack.
        assert_eq!(one(spec(RX, PROGRAM_END - 0x1000, b"x", 0x1001)), Err(ElfError::OutsideProgramArea));
        assert_eq!(one(spec(RX, CODE, b"x", u64::MAX)), Err(ElfError::OutsideProgramArea));
        assert_eq!(one(spec(RX, CODE, b"xyz", 2)), Err(ElfError::BadSize));
        assert_eq!(one(spec(RX | PF_W, CODE, b"x", 1)), Err(ElfError::WritableAndExecutable));
    }

    #[test]
    fn segments_may_not_share_a_page() {
        let file = build(CODE, &[spec(RX, CODE, b"x", 0x1001), spec(RW, DATA, b"d", 1)]);
        assert_eq!(parse(&file), Err(ElfError::Overlapping));
    }

    #[test]
    fn the_entry_point_must_be_in_code() {
        let data_entry = build(DATA, &[spec(RX, CODE, b"x", 1), spec(RW, DATA, b"d", 1)]);
        assert_eq!(parse(&data_entry), Err(ElfError::BadEntry));
        let past_code = build(CODE + 8, &[spec(RX, CODE, b"x", 8)]);
        assert_eq!(parse(&past_code), Err(ElfError::BadEntry));
    }

    #[test]
    fn truncated_files_are_refused() {
        let file = typical();
        // Cut inside the program headers, then inside the segment data.
        assert_eq!(parse(&file[..HEADER_SIZE + 10]), Err(ElfError::Truncated));
        assert_eq!(parse(&file[..file.len() - 2]), Err(ElfError::Truncated));
        assert_eq!(parse(&build(CODE, &[])), Err(ElfError::NoSegments));
    }
}
