// panic.rs
//! Panic handler: report over UART, blink the status LED, then reboot.

use core::panic::PanicInfo;
use core::time::Duration;
use crate::board::STATUS_LED;
use crate::drivers::gpio::{set_pin_mode, write_pin, Pin, PinMode};
use crate::println;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("{}", info);

    if let Some(led) = Pin::new(STATUS_LED) {
        set_pin_mode(led, PinMode::Output);
        for _ in 0..10 {
            write_pin(led, false);
            super::sleep(Duration::from_millis(100));
            write_pin(led, true);
            super::sleep(Duration::from_millis(100));
        }
    }

    println!("Rebooting the Raspberry PI");
    super::reboot()
}
