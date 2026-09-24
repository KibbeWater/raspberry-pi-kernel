// commands.rs
//! Commands typed on the host, delivered by the Arduino bridge as `MSG` frames.
//! Commands write their answer into a `session::Reply`; `help` adds one `HELP` line per
//! command so the list never outgrows a single frame.

use alloc::boxed::Box;
use alloc::vec::Vec;
use crate::board::STATUS_LED;
use crate::drivers::gpio::{set_pin_mode, write_pin, Pin, PinMode};
use crate::session::{LineKind, Reply};
use crate::sys;

/// Usage and description of every command, listed by `help`. Keep in sync with
/// `Shell::handle`.
const COMMANDS: &[(&str, &str)] = &[
    ("help", "list commands"),
    ("led on|off|toggle", "switch the status LED"),
    ("uptime", "time since reset"),
    ("version", "git commit the kernel was built from"),
    ("info", "board revision, SoC temperature, EL and MMU"),
    ("heap [test]", "heap usage, or run an allocator stress test"),
    ("reboot", "reset the board"),
    ("shutdown", "halt; pull GPIO3 low to boot again"),
    ("panic [msg]", "panic, blink the LED and reboot"),
    ("fault", "read unmapped memory to test the exception handler"),
];

pub struct Shell {
    led: Pin,
    led_on: bool,
}

/// Something a command does after its reply has been sent, because it ends the kernel.
/// Replying first stops the Uno from retransmitting the command to the rebooted Pi.
pub enum Action<'a> {
    Reboot,
    Shutdown,
    Panic(&'a str),
    Fault,
}

impl Action<'_> {
    pub fn perform(self) {
        match self {
            Action::Reboot => sys::reboot(),
            Action::Shutdown => sys::shutdown(),
            Action::Panic(message) => panic!("{}", message),
            Action::Fault => {
                // Nothing is mapped above 2GB, so this raises a data abort.
                let value = unsafe { core::ptr::read_volatile(0xDEAD_0000 as *const u32) };
                panic!("read {:#x} from unmapped memory, expected a fault", value);
            }
        }
    }
}

impl Shell {
    pub fn new() -> Self {
        let led = Pin::new(STATUS_LED).expect("Invalid GPIO pin");
        set_pin_mode(led, PinMode::Output);
        write_pin(led, false);
        Shell { led, led_on: false }
    }

    pub fn handle<'a>(&mut self, text: &'a str, reply: &mut Reply) -> Option<Action<'a>> {
        let text = text.trim();
        match text {
            "help" => {
                for (usage, description) in COMMANDS {
                    reply.line(LineKind::Help, format_args!("{:<18} {}", usage, description));
                }
            }
            "led on" => self.set_led(true, reply),
            "led off" => self.set_led(false, reply),
            "led toggle" => self.set_led(!self.led_on, reply),
            "uptime" => {
                reply.line(LineKind::Rsp, format_args!("uptime {}s", sys::uptime().as_secs()))
            }
            "version" => reply.line(LineKind::Rsp, format_args!("RustyPI {}", sys::VERSION)),
            "info" => info(reply),
            "heap" => heap(reply),
            "heap test" => heap_test(reply),
            "reboot" => {
                reply.rsp("rebooting");
                return Some(Action::Reboot);
            }
            "shutdown" => {
                reply.rsp("shutting down, pull GPIO3 low to boot again");
                return Some(Action::Shutdown);
            }
            "panic" => {
                reply.rsp("panicking");
                return Some(Action::Panic("panic requested over link"));
            }
            _ if text.starts_with("panic ") => {
                reply.rsp("panicking");
                return Some(Action::Panic(text["panic ".len()..].trim()));
            }
            "fault" => {
                reply.rsp("reading unmapped memory");
                return Some(Action::Fault);
            }
            _ => reply.line(LineKind::Rsp, format_args!("echo: {}", text)),
        }
        None
    }

    fn set_led(&mut self, on: bool, reply: &mut Reply) {
        self.led_on = on;
        write_pin(self.led, on);
        reply.rsp(if on { "led is on" } else { "led is off" });
    }
}

fn heap(reply: &mut Reply) {
    let stats = sys::heap_stats();
    reply.line(LineKind::Rsp, format_args!(
        "heap {} of {} KB used, largest free block {} KB",
        stats.used / 1024,
        stats.total / 1024,
        stats.largest_free / 1024,
    ));
}

/// Allocates, checks and frees a spread of block sizes and alignments, then checks that
/// the heap is back where it started with no fragments left behind.
fn heap_test(reply: &mut Reply) {
    let before = sys::heap_stats();

    let mut blocks: Vec<Vec<u8>> = Vec::new();
    for i in 0..200usize {
        let size = 1 + (i * 37) % 3000;
        blocks.push((0..size).map(|j| (i + j) as u8).collect());
    }
    // Free every other block to leave holes, then fill them with differently sized blocks.
    for i in (0..blocks.len()).step_by(2) {
        blocks[i] = Vec::new();
    }
    for i in (0..blocks.len()).step_by(2) {
        let size = 1 + (i * 53) % 1500;
        blocks[i] = (0..size).map(|j| (i + j) as u8).collect();
    }
    let aligned: Vec<Box<Aligned>> = (0..16).map(|i| Box::new(Aligned(i))).collect();

    let corrupt = blocks.iter().enumerate().any(|(i, block)| {
        block.iter().enumerate().any(|(j, &byte)| byte != (i + j) as u8)
    }) || aligned.iter().enumerate().any(|(i, b)| b.0 != i as u8);
    let misaligned = aligned.iter().any(|b| (&**b as *const Aligned as usize) % 256 != 0);
    let peak = sys::heap_stats().used;
    drop(blocks);
    drop(aligned);
    let after = sys::heap_stats();

    let ok = !corrupt && !misaligned && after.used == before.used
        && after.largest_free == before.largest_free;
    reply.line(LineKind::Rsp, format_args!(
        "heap test {}: peak {} KB{}{}{}",
        if ok { "passed" } else { "FAILED" },
        peak / 1024,
        if corrupt { ", data corrupted" } else { "" },
        if misaligned { ", bad alignment" } else { "" },
        if after.used != before.used || after.largest_free != before.largest_free {
            ", memory not fully returned"
        } else {
            ""
        },
    ));
}

#[repr(align(256))]
struct Aligned(u8);

fn info(reply: &mut Reply) {
    let el = sys::exception_level();
    let mmu = if sys::mmu_enabled() { "on" } else { "off" };
    match (sys::board_revision(), sys::temperature()) {
        (Some(revision), Some(millidegrees)) => reply.line(LineKind::Rsp, format_args!(
            "board rev {:#x}, soc {}.{} C, EL{}, mmu {}",
            revision,
            millidegrees / 1000,
            millidegrees % 1000 / 100,
            el,
            mmu,
        )),
        _ => reply.line(LineKind::Rsp, format_args!("firmware did not answer, EL{}, mmu {}", el, mmu)),
    }
}
