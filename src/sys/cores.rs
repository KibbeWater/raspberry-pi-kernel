// cores.rs
//! Starting cores 1 to 3. The firmware leaves them in a spin loop, each watching its own
//! word of a table in low memory (with its caches off) for an address to jump to at EL2.
//! Each then turns on its MMU, becomes its own idle task and starts its timer tick; from then
//! on it runs whatever tasks are ready, like core 0.

use alloc::vec;
use alloc::boxed::Box;
use core::arch::asm;
use core::ptr::write_volatile;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use crate::board::CORES;
use crate::{arch, sched};

/// Where core n looks for its start address: `SPIN_TABLE + 8 * n`.
const SPIN_TABLE: usize = 0xD8;

/// Each core's first stack, which its idle task keeps.
const STACK_SIZE: usize = 32 * 1024;

/// How long the cores get to report in.
const START_TIMEOUT: Duration = Duration::from_millis(100);

/// The top of each core's first stack, read by `boot.s` before its caches are on. One cache
/// line, so one clean puts it all in memory.
#[repr(C, align(64))]
struct StackTops([usize; CORES]);

#[no_mangle]
static mut SECONDARY_STACK_TOPS: StackTops = StackTops([0; CORES]);

/// Cores running, core 0 included.
static RUNNING: AtomicUsize = AtomicUsize::new(1);

/// Writes the cache line holding `addr` back to memory, for a core whose caches are off.
fn clean_to_memory(addr: usize) {
    unsafe { asm!("dc civac, {}", in(reg) addr, options(nostack, preserves_flags)) };
}

/// Starts cores 1 to 3 and waits for them to report in. Returns how many cores are running.
/// Call on core 0, once the scheduler and its timer tick are going.
pub fn start() -> usize {
    extern "C" {
        fn _secondary_start();
    }
    let tops = &raw mut SECONDARY_STACK_TOPS;
    for core in 1..CORES {
        // Kept for good: it becomes the core's idle task's stack.
        let stack = Box::leak(vec![0u8; STACK_SIZE].into_boxed_slice());
        unsafe { (*tops).0[core] = (stack.as_mut_ptr() as usize + STACK_SIZE) & !15 };
    }
    clean_to_memory(tops as usize);
    for core in 1..CORES {
        let slot = SPIN_TABLE + 8 * core;
        unsafe { write_volatile(slot as *mut u64, _secondary_start as usize as u64) };
        clean_to_memory(slot);
    }
    // Everything in memory before the cores wake from their `wfe`.
    unsafe { asm!("dsb sy", "sev", options(nostack)) };

    let start = super::uptime();
    while RUNNING.load(Ordering::Acquire) < CORES && super::uptime() - start < START_TIMEOUT {
        core::hint::spin_loop();
    }
    RUNNING.load(Ordering::Acquire)
}

/// Where cores 1 to 3 go from `boot.s`, on their first stack.
#[no_mangle]
extern "C" fn secondary_main() -> ! {
    // First, like on core 0: until then all memory is Device memory, with no atomics.
    arch::mmu::turn_on();
    sched::start_core();
    super::start_tick();
    RUNNING.fetch_add(1, Ordering::Release);
    sched::idle()
}
