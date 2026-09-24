// system.rs
//! Commands about the system as a whole: time, version, board info, power and crash tests.

use rustypi_core::session::{LineKind, Reply};
use super::{Command, Outcome};
use crate::sys;

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

pub const COMMANDS: &[Command] = &[
    Command { name: "uptime", args: "", description: "time since reset", run: uptime },
    Command { name: "version", args: "", description: "git commit the kernel was built from", run: version },
    Command { name: "info", args: "", description: "board, firmware, memory, temperature, EL, MMU", run: info },
    Command { name: "echo", args: "<text>", description: "reply with the text", run: echo },
    Command { name: "reboot", args: "", description: "reset the board", run: reboot },
    Command { name: "shutdown", args: "", description: "halt; pull GPIO3 low to boot again", run: shutdown },
    Command { name: "panic", args: "[msg]", description: "panic, blink the LED and reboot", run: panic },
    Command { name: "fault", args: "", description: "read unmapped memory to test exceptions", run: fault },
];

fn uptime<'a>(_: &mut super::Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    if !args.is_empty() {
        return Outcome::Usage;
    }
    reply.line(LineKind::Rsp, format_args!("uptime {}s", sys::uptime().as_secs()));
    Outcome::Done
}

fn version<'a>(_: &mut super::Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    if !args.is_empty() {
        return Outcome::Usage;
    }
    reply.line(LineKind::Rsp, format_args!("RustyPI {}", sys::VERSION));
    Outcome::Done
}

fn info<'a>(_: &mut super::Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    if !args.is_empty() {
        return Outcome::Usage;
    }
    let el = sys::exception_level();
    let mmu = if sys::mmu_enabled() { "on" } else { "off" };
    match sys::board_info() {
        Ok(info) => {
            reply.line(LineKind::Rsp, format_args!(
                "board rev {:#x}, firmware {}, arm memory {} MB",
                info.revision,
                info.firmware,
                info.arm_memory / (1024 * 1024),
            ));
            reply.line(LineKind::Rsp, format_args!(
                "soc {}.{} C, EL{}, mmu {}",
                info.millidegrees / 1000,
                info.millidegrees % 1000 / 100,
                el,
                mmu,
            ));
        }
        Err(error) => {
            reply.line(LineKind::Rsp, format_args!("mailbox: {}", error));
            reply.line(LineKind::Rsp, format_args!("EL{}, mmu {}", el, mmu));
        }
    }
    Outcome::Done
}

fn echo<'a>(_: &mut super::Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    reply.rsp(args);
    Outcome::Done
}

fn reboot<'a>(_: &mut super::Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    if !args.is_empty() {
        return Outcome::Usage;
    }
    reply.rsp("rebooting");
    Outcome::Then(Action::Reboot)
}

fn shutdown<'a>(_: &mut super::Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    if !args.is_empty() {
        return Outcome::Usage;
    }
    reply.rsp("shutting down, pull GPIO3 low to boot again");
    Outcome::Then(Action::Shutdown)
}

fn panic<'a>(_: &mut super::Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    reply.rsp("panicking");
    Outcome::Then(Action::Panic(if args.is_empty() { "panic requested over link" } else { args }))
}

fn fault<'a>(_: &mut super::Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    if !args.is_empty() {
        return Outcome::Usage;
    }
    reply.rsp("reading unmapped memory");
    Outcome::Then(Action::Fault)
}
