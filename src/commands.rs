// commands.rs
//! Commands typed on the host, delivered by the Arduino bridge as `MSG` frames.
//! Every command answers with an `RSP` frame, except `help`, which sends one `HELP`
//! frame per command so the list never outgrows a single line.

use crate::board::STATUS_LED;
use crate::drivers::gpio::{set_pin_mode, write_pin, Pin, PinMode};
use crate::{link, sys};

/// Usage and description of every command, listed by `help`. Keep in sync with
/// `Shell::handle`.
const COMMANDS: &[(&str, &str)] = &[
    ("help", "list commands"),
    ("led on|off|toggle", "switch the status LED"),
    ("uptime", "time since reset"),
    ("version", "git commit the kernel was built from"),
    ("info", "board revision, SoC temperature, EL and MMU"),
    ("reboot", "reset the board"),
    ("shutdown", "halt; pull GPIO3 low to boot again"),
    ("panic [msg]", "panic, blink the LED and reboot"),
    ("fault", "read unmapped memory to test the exception handler"),
];

pub struct Shell {
    led: Pin,
    led_on: bool,
}

impl Shell {
    pub fn new() -> Self {
        let led = Pin::new(STATUS_LED).expect("Invalid GPIO pin");
        set_pin_mode(led, PinMode::Output);
        write_pin(led, false);
        Shell { led, led_on: false }
    }

    pub fn handle(&mut self, text: &str) {
        let text = text.trim();
        match text {
            "help" => {
                for (usage, description) in COMMANDS {
                    link::send_fmt("HELP", format_args!("{:<18} {}", usage, description));
                }
            }
            "led on" => self.set_led(true),
            "led off" => self.set_led(false),
            "led toggle" => self.set_led(!self.led_on),
            "uptime" => {
                link::send_fmt("RSP", format_args!("uptime {}s", sys::uptime().as_secs()))
            }
            "version" => link::send_fmt("RSP", format_args!("RustyPI {}", sys::VERSION)),
            "info" => info(),
            "reboot" => {
                link::send("RSP", "rebooting");
                sys::reboot();
            }
            "shutdown" => {
                link::send("RSP", "shutting down, pull GPIO3 low to boot again");
                sys::shutdown();
            }
            "panic" => panic!("panic requested over link"),
            _ if text.starts_with("panic ") => panic!("{}", text["panic ".len()..].trim()),
            "fault" => {
                // Nothing is mapped above 2GB, so this raises a data abort.
                let value = unsafe { core::ptr::read_volatile(0xDEAD_0000 as *const u32) };
                link::send_fmt("RSP", format_args!("read {:#x}, expected a fault", value));
            }
            _ => link::send_fmt("RSP", format_args!("echo: {}", text)),
        }
    }

    fn set_led(&mut self, on: bool) {
        self.led_on = on;
        write_pin(self.led, on);
        link::send("RSP", if on { "led is on" } else { "led is off" });
    }
}

fn info() {
    let el = sys::exception_level();
    let mmu = if sys::mmu_enabled() { "on" } else { "off" };
    match (sys::board_revision(), sys::temperature()) {
        (Some(revision), Some(millidegrees)) => link::send_fmt("RSP", format_args!(
            "board rev {:#x}, soc {}.{} C, EL{}, mmu {}",
            revision,
            millidegrees / 1000,
            millidegrees % 1000 / 100,
            el,
            mmu,
        )),
        _ => link::send_fmt("RSP", format_args!("firmware did not answer, EL{}, mmu {}", el, mmu)),
    }
}
