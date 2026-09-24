// led.rs
//! The status LED.

use rustypi_core::session::Reply;
use super::{Command, Outcome, Shell};
use crate::board::STATUS_LED;
use crate::drivers::gpio::{set_pin_mode, write_pin, Pin, PinMode};

pub struct Led {
    pin: Pin,
    on: bool,
}

impl Led {
    pub fn new() -> Self {
        let pin = Pin::new(STATUS_LED).expect("Invalid GPIO pin");
        set_pin_mode(pin, PinMode::Output);
        write_pin(pin, false);
        Led { pin, on: false }
    }

    fn set(&mut self, on: bool) {
        self.on = on;
        write_pin(self.pin, on);
    }
}

pub const COMMANDS: &[Command] = &[
    Command { name: "led", args: "on|off|toggle", description: "switch the status LED", run: led },
];

fn led<'a>(shell: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    let on = match args {
        "on" => true,
        "off" => false,
        "toggle" => !shell.led.on,
        _ => return Outcome::Usage,
    };
    shell.led.set(on);
    reply.rsp(if on { "led is on" } else { "led is off" });
    Outcome::Done
}
