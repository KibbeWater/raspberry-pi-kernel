// panic.rs
//! Panic handler: stop the other cores, report over UART, blink the status LED, then reboot.

use core::arch::asm;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use crate::board::STATUS_LED;
use crate::drivers::gpio::{set_pin_mode, write_pin, Pin, PinMode};
use core::fmt::Write;
use crate::drivers::uart::UartWriter;
use crate::{arch, println};

/// The core that panicked, plus one; 0 while none has.
static PANICKED: AtomicUsize = AtomicUsize::new(0);

/// Called on every interrupt: if another core panicked, this one stops here for good, so the
/// report isn't mixed with other output and nothing carries on with the kernel broken. The
/// others' next timer tick (at most a tick away) brings them here.
pub fn stop_if_another_core_panicked() {
    let panicked = PANICKED.load(Ordering::Relaxed);
    if panicked != 0 && panicked != arch::core_id() + 1 {
        arch::irq_disable();
        loop {
            unsafe { asm!("wfe", options(nomem, nostack)) };
        }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // The first core to panic reports; one panicking after it just stops.
    let me = arch::core_id() + 1;
    if let Err(first) = PANICKED.compare_exchange(0, me, Ordering::Relaxed, Ordering::Relaxed) {
        if first != me {
            stop_if_another_core_panicked();
        }
        // This core panicked again while reporting (the kernel is too broken to format the
        // report): say so without formatting anything, and reboot, keeping the first report
        // readable.
        let _ = UartWriter.write_str("\npanicked again while reporting a panic\n");
        super::reboot();
    }
    println!("{}", info);

    if let Some(led) = Pin::new(STATUS_LED) {
        set_pin_mode(led, PinMode::Output);
        for _ in 0..10 {
            write_pin(led, false);
            super::delay(Duration::from_millis(100));
            write_pin(led, true);
            super::delay(Duration::from_millis(100));
        }
    }

    println!("Rebooting the Raspberry PI");
    super::reboot()
}
