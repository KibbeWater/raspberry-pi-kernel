#![no_std]
#![no_main]

mod drivers;
mod libs;
mod synchronization;

use core::arch::{asm, global_asm};
use drivers::gpio::{write_pin, set_pin_mode, PinMode, Pin};
use drivers::link::{self, Link};
use drivers::time::{sleep, system_time};
use drivers::uart::Uart;

use core::panic::{PanicInfo};

global_asm!(include_str!("boot.s"));

#[link_section=".text._start"]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    sleep(1000);
    Uart::init(link::BAUD);
    Uart::send_string("Hello from RPi!\n");
    link::send("HELLO", "RustyPI");

    let led = Pin::new(21).expect("Invalid GPIO pin");
    set_pin_mode(led, PinMode::Output);
    let mut led_on = false;
    write_pin(led, led_on);

    // Poll continuously: the PL011 RX FIFO only holds 16 bytes (~4ms at 38400 baud),
    // so nothing in this loop may block.
    let mut link = Link::new();
    loop {
        link.poll(|kind, payload| match kind {
            "PING" => link::send("PONG", payload),
            "HELLO" => link::send("HELLO", "RustyPI"),
            "MSG" => handle_command(payload, led, &mut led_on),
            _ => link::send_parts("ERR", &["unknown kind ", kind]),
        });
    }
}

/// Handles a `MSG` frame from the Arduino and answers with an `RSP` frame.
fn handle_command(text: &str, led: Pin, led_on: &mut bool) {
    let text = text.trim();
    match text {
        "help" => link::send("RSP", "commands: help, led on, led off, led toggle, uptime"),
        "led on" | "led off" | "led toggle" => {
            *led_on = match text {
                "led on" => true,
                "led off" => false,
                _ => !*led_on,
            };
            write_pin(led, *led_on);
            link::send("RSP", if *led_on { "led is on" } else { "led is off" });
        }
        "uptime" => {
            // The 32-bit microsecond counter wraps every ~71 minutes.
            let mut buf = [0; 10];
            let secs = link::fmt_u32(system_time() / 1_000_000, &mut buf);
            link::send_parts("RSP", &["uptime ", secs, "s"]);
        }
        _ => link::send_parts("RSP", &["echo: ", text]),
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