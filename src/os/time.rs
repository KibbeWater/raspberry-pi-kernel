const SYSTEM_TIME_BASE: usize = 0x3F003000;
const SYSTEM_TIME_COUNTER_LOW_OFFSET: usize = 0x4;

const CM_BASE: usize = 0x3F101000;

#[repr(C)]
struct CmRegisters {
    control: u32,
    status: u32,
    divisor: u32,
    test: u32,
    reserved: [u32; 4],
}

impl CmRegisters {
    fn instance() -> &'static mut Self {
        unsafe { &mut *(CM_BASE as *mut Self) }
    }
}

pub fn system_time() -> u32 {
    let timer_ptr = (SYSTEM_TIME_BASE + SYSTEM_TIME_COUNTER_LOW_OFFSET) as *const u32;
    // Read the timer value using a volatile read to ensure we actually fetch the register.
    unsafe { core::ptr::read_volatile(timer_ptr) }
}

pub fn clock_frequency() -> u32 {
    let cm = CmRegisters::instance();
    // Assuming `divisor` holds the frequency of interest (this varies depending on exact clock source)
    cm.divisor
}

/// Pauses execution for a set amount of time
pub fn sleep(ms: u32) {
    let delay_us = ms * 1000;
    let start = system_time();

    loop {
        // Use wrapping subtraction in case the counter overflows.
        let elapsed = system_time().wrapping_sub(start);
        if elapsed >= delay_us {
            break;
        }
    }
}