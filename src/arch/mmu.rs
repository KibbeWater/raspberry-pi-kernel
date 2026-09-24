// mmu.rs
//! Identity-maps the low 2GB for the kernel and turns on the MMU and caches.
//!
//! RAM is mapped as Normal cacheable memory and the peripherals as Device memory, so
//! unaligned accesses to RAM are allowed from here on. Uses a 4KB granule with a 32-bit
//! address space: one level 1 table (1GB entries) and one level 2 table (2MB blocks) for
//! the first gigabyte.
//!
//! Kernel mappings are global and out of reach of user programs: EL0 may neither access nor
//! execute them. Each program's address space (`rustypi_core::paging`) copies the kernel's
//! level 1 entries and adds its user window; the kernel's own table maps no user memory.

use core::arch::asm;
use crate::board::{LOCAL_PERIPHERAL_BASE, PERIPHERAL_BASE};

use rustypi_core::paging::Table;

const BLOCK_2M: usize = 1 << 21;

static mut LEVEL1: Table = Table([0; 512]);
static mut LEVEL2: Table = Table([0; 512]);

// Descriptor bits.
const DESC_BLOCK: u64 = 0b01;
const DESC_TABLE: u64 = 0b11;
const ATTR_NORMAL: u64 = 0 << 2; // MAIR index 0
const ATTR_DEVICE: u64 = 1 << 2; // MAIR index 1
const ATTR_NORMAL_UNCACHED: u64 = 2 << 2; // MAIR index 2
const INNER_SHAREABLE: u64 = 3 << 8;
const ACCESS_FLAG: u64 = 1 << 10;
const PRIVILEGED_EXECUTE_NEVER: u64 = 1 << 53; // PXN
const USER_EXECUTE_NEVER: u64 = 1 << 54; // UXN
const EXECUTE_NEVER: u64 = PRIVILEGED_EXECUTE_NEVER | USER_EXECUTE_NEVER;

const NORMAL: u64 = DESC_BLOCK | ATTR_NORMAL | INNER_SHAREABLE | ACCESS_FLAG | USER_EXECUTE_NEVER;

const DEVICE: u64 = DESC_BLOCK | ATTR_DEVICE | ACCESS_FLAG | EXECUTE_NEVER;
const NORMAL_UNCACHED: u64 = DESC_BLOCK | ATTR_NORMAL_UNCACHED | INNER_SHAREABLE | ACCESS_FLAG | EXECUTE_NEVER;

/// Index 0: Normal, write-back read/write-allocate. Index 1: Device-nGnRE.
/// Index 2: Normal, non-cacheable (writes can still be combined).
const MAIR: u64 = 0xFF | 0x04 << 8 | 0x44 << 16;

const CACHE_LINE: usize = 64;

/// T0SZ=32 (4GB), table walks write-back cacheable and inner shareable, 4KB granule,
/// TTBR1 walks disabled (EPD1). The ASID comes from TTBR0 (A1=0) and is 8 bits (AS=0).
const TCR: u64 = 32 | 1 << 8 | 1 << 10 | 3 << 12 | 1 << 23;

/// SCTLR_EL1: M (MMU), C (data cache), I (instruction cache).
const SCTLR_ENABLE: u64 = 1 << 0 | 1 << 2 | 1 << 12;

/// Builds the page tables and turns on the MMU and caches.
///
/// Must run before anything that might make an unaligned access: until then all
/// memory is Device memory, where unaligned accesses fault.
pub fn enable() {
    unsafe {
        let level2 = &raw mut LEVEL2;
        for (i, entry) in (*level2).0.iter_mut().enumerate() {
            let addr = i * BLOCK_2M;
            let attrs = if addr >= PERIPHERAL_BASE { DEVICE } else { NORMAL };
            *entry = addr as u64 | attrs;
        }

        let level1 = &raw mut LEVEL1;
        (*level1).0[0] = level2 as u64 | DESC_TABLE;
        // ARM local peripherals (core timers, mailboxes, interrupt routing).
        (*level1).0[1] = LOCAL_PERIPHERAL_BASE as u64 | DEVICE;

        asm!(
            "msr mair_el1, {mair}",
            "msr tcr_el1, {tcr}",
            "msr ttbr0_el1, {ttbr}",
            "dsb ish",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            "mrs {tmp}, sctlr_el1",
            "orr {tmp}, {tmp}, {enable}",
            "msr sctlr_el1, {tmp}",
            "isb",
            mair = in(reg) MAIR,
            tcr = in(reg) TCR,
            ttbr = in(reg) level1 as u64,
            enable = in(reg) SCTLR_ENABLE,
            tmp = out(reg) _,
            options(nostack),
        );
    }
}

/// Remaps the 2MB blocks covering `start..start + len` as uncached Normal memory, so
/// memory shared with the GPU (like the framebuffer) sees writes without cache
/// maintenance.
///
/// Panics if the blocks would include the kernel's own memory (image, stack or heap) or
/// reach the peripherals: uncached kernel memory would still work, but slowly and by
/// accident.
pub fn make_uncached(start: usize, len: usize) {
    extern "C" {
        static __end: u8;
    }
    let kernel_end = &raw const __end as usize;
    let first = start / BLOCK_2M;
    let last = (start + len).div_ceil(BLOCK_2M);
    if first * BLOCK_2M < kernel_end || last * BLOCK_2M > PERIPHERAL_BASE {
        panic!("mmu: refusing to make {:#x}..{:#x} uncached", start, start + len);
    }

    unsafe {
        let level2 = &raw mut LEVEL2;
        // Break-before-make: changing a live mapping's attributes requires removing it and
        // flushing the TLB before installing the new one.
        for i in first..last {
            (*level2).0[i] = 0;
        }
        asm!("dsb ishst", "tlbi vmalle1", "dsb ish", "isb", options(nostack));
        for i in first..last {
            (*level2).0[i] = (i * BLOCK_2M) as u64 | NORMAL_UNCACHED;
        }
        asm!("dsb ishst", "isb", options(nostack));

        // Drop anything the CPU cached (or prefetched) under the old mapping.
        for line in (first * BLOCK_2M..last * BLOCK_2M).step_by(CACHE_LINE) {
            asm!("dc civac, {}", in(reg) line, options(nostack, preserves_flags));
        }
        asm!("dsb sy", options(nostack));
    }
}

/// The kernel's level 1 table, which program address spaces copy their kernel entries from.
pub fn kernel_table() -> &'static Table {
    unsafe { &*&raw const LEVEL1 }
}

/// The TTBR0 value for the kernel's own address space, ASID 0: no user memory mapped.
pub fn kernel_translation_base() -> u64 {
    &raw const LEVEL1 as u64
}

/// Switches TTBR0 to another address space. The TLB keeps entries tagged with the old ASID,
/// so no flush is needed while ASIDs are unique.
pub fn set_translation_base(ttbr0: u64) {
    unsafe {
        // Page table writes must be visible to the table walker before it can use them.
        asm!("dsb ish", "msr ttbr0_el1, {}", "isb", in(reg) ttbr0, options(nostack));
    }
}

/// Forgets every cached translation, for every ASID. Needed before freeing an address space's
/// tables, or giving its ASID to another.
pub fn flush_tlb() {
    unsafe { asm!("dsb ishst", "tlbi vmalle1", "dsb ish", "isb", options(nostack)) };
}

/// Makes code just written to physical memory at `start..start + len` safe to execute:
/// cleans it from the data cache to where instruction fetches see it, then drops the whole
/// instruction cache, which may hold stale lines under other virtual addresses.
pub fn sync_instruction_cache(start: usize, len: usize) {
    unsafe {
        for line in (start & !(CACHE_LINE - 1)..start + len).step_by(CACHE_LINE) {
            asm!("dc cvau, {}", in(reg) line, options(nostack, preserves_flags));
        }
        asm!("dsb ish", "ic iallu", "dsb ish", "isb", options(nostack));
    }
}

/// Whether the MMU is on.
pub fn is_enabled() -> bool {
    let sctlr: u64;
    unsafe { asm!("mrs {}, sctlr_el1", out(reg) sctlr, options(nomem, nostack)) };
    sctlr & 1 != 0
}
