#![no_std]
#![no_main]

mod drivers;
mod libs;
mod synchronization;

use core::arch::{asm, global_asm};
use drivers::gpio::{write_pin, set_pin_mode, PinMode, Pin};
use drivers::time::sleep;
use drivers::uart::Uart;

use core::panic::{PanicInfo};

global_asm!(include_str!("boot.s"));

#[link_section=".text._start"]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    sleep(1000);
    Uart::init();
    sleep(1000);
    Uart::send_string("Hello from RPi!\n");
    sleep(1000);

    let led = Pin::new(21).expect("Invalid GPIO pin");

    set_pin_mode(led, PinMode::Output);

    let mut i: u32 = 1;
    loop {
        if i % 10 == 0 {
            Uart::send_string("I have iterated 10 times!\n");
            panic!("Example kernel panic !!!");
        }

        write_pin(led, true);
        Uart::send_string("LED is currently 1!\n");
        sleep(500);
        write_pin(led, false);
        Uart::send_string("LED is currently 0!\n");
        sleep(500);

        i += 1;
    }
}

#[panic_handler]
fn _panic(_info: &PanicInfo) -> ! {
    let message = _info.message()
        .as_str()
        .unwrap_or("Unexpected error occurred");

    Uart::send_string(message);
    Uart::send_string("\n");

    let led = Pin::new(21).expect("Invalid GPIO pin");
    set_pin_mode(led, PinMode::Output);

    for _ in 1..10 {
        write_pin(led, false);
        sleep(100);
        write_pin(led, true);
        sleep(100);
    }

    Uart::send_string("Rebooting the Raspberry PI\n");
    unsafe { asm!("bl _start"); } // Reboot

    loop {}
}