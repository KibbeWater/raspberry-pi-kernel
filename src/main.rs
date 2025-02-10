#![no_std]
#![no_main]

mod drivers;

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

use drivers::gpio::{write_pin, set_pin_mode, PinMode, Pin};
use drivers::time::sleep;
use drivers::uart::{uart_init, uart_send_string, uart_read_line};

global_asm!(include_str!("boot.s"));

#[link_section=".text._start"]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    sleep(1000);
    uart_init();
    sleep(1000);
    uart_send_string("Hello from RPi!\n");
    sleep(1000);
    
    let led = Pin::new(21).expect("Invalid GPIO pin");

    set_pin_mode(led, PinMode::Output);

    let mut i: u32 = 1;
    loop {
        if i % 10 == 0 {
            uart_send_string("I have iterated 10 times!\nPlease provide a panic message:\n");

            let mut buffer = [0u8; 128];
            let input = uart_read_line(&mut buffer);

            uart_send_string("You sent: ");
            uart_send_string(input);
            uart_send_string("\n");
            
            panic!("{}", input);
        }

        write_pin(led, true);
        uart_send_string("LED is currently 1!\n");
        sleep(500);
        write_pin(led, false);
        uart_send_string("LED is currently 0!\n");
        sleep(500);

        i += 1;
    }
}

#[panic_handler]
fn _panic(_info: &PanicInfo) -> ! {
    uart_init();

    let message = _info.message()
        .as_str()
        .unwrap_or("Unexpected error occurred");
    
    uart_send_string(message);
    uart_send_string("\n");
    
    let led = Pin::new(21).expect("Invalid GPIO pin");
    set_pin_mode(led, PinMode::Output);

    for _ in 1..10 {
        write_pin(led, false);
        sleep(100);
        write_pin(led, true);
        sleep(100);
    }
    
    uart_send_string("Rebooting the Raspberry PI\n");
    unsafe { asm!("bl _start"); } // Reboot
    
    loop {}
}
