// programs.rs
//! Built-in test programs, until programs can be loaded from the SD card.
//!
//! They are hand-written assembly (`programs.s`), because Rust compiled into the kernel
//! would call into kernel memory (for formatting, panics, memcpy...), which EL0 can't run.

use core::arch::global_asm;
use rustypi_abi::{Errno, Number};
use super::Exit;

global_asm!(
    include_str!("programs.s"),
    EXIT = const Number::Exit as u64,
    WRITE = const Number::Write as u64,
    SLEEP = const Number::Sleep as u64,
    UPTIME = const Number::Uptime as u64,
    ENOSYS = const Errno::NoSys.code(),
    EINVAL = const Errno::Invalid.code(),
    EFAULT = const Errno::Fault.code(),
);

extern "C" {
    static user_image_start: u8;
    static user_image_end: u8;
    fn user_hello();
    fn user_ticker();
    fn user_abi();
    fn user_spin();
    fn user_fault();
    fn user_privileged();
    fn user_float();
    fn user_isolated();
    fn user_overflow();
    fn user_write_code();
    fn user_run_stack();
}

/// ESR_EL1 exception classes the crashing programs should die of.
const CLASS_UNKNOWN: u64 = 0x00;
const CLASS_FP: u64 = 0x07;
const CLASS_INSTRUCTION_ABORT_EL0: u64 = 0x20;
const CLASS_DATA_ABORT_EL0: u64 = 0x24;

pub struct Program {
    pub name: &'static str,
    pub description: &'static str,
    /// How it ends when the kernel works.
    pub expected: Expected,
    /// Its code in the kernel image. It runs from a copy in its own address space.
    code: unsafe extern "C" fn(),
}

impl Program {
    /// Where it starts, from the start of `image`.
    pub fn offset(&self) -> usize {
        self.code as usize - &raw const user_image_start as usize
    }
}

#[derive(Clone, Copy)]
pub enum Expected {
    Exits(i32),
    /// Killed by a fault of this exception class.
    Crashes(u64),
}

impl Expected {
    pub fn matches(self, exit: Exit) -> bool {
        match (self, exit) {
            (Expected::Exits(expected), Exit::Code(code)) => code == expected,
            (Expected::Crashes(class), Exit::Crashed(fault)) => fault.class() == class,
            _ => false,
        }
    }
}

pub const PROGRAMS: &[Program] = &[
    Program { name: "hello", description: "print a line and exit", expected: Expected::Exits(0), code: user_hello },
    Program {
        name: "ticker",
        description: "tick 3 times, checking registers survive sleeps",
        expected: Expected::Exits(0),
        code: user_ticker,
    },
    Program {
        name: "abi",
        description: "check bad pointers and arguments are refused",
        expected: Expected::Exits(0),
        code: user_abi,
    },
    Program { name: "spin", description: "busy-loop, to be preempted", expected: Expected::Exits(0), code: user_spin },
    Program {
        name: "fault",
        description: "read address 0",
        expected: Expected::Crashes(CLASS_DATA_ABORT_EL0),
        code: user_fault,
    },
    Program {
        name: "privileged",
        description: "read a kernel system register",
        expected: Expected::Crashes(CLASS_UNKNOWN),
        code: user_privileged,
    },
    Program {
        name: "float",
        description: "use a floating point register",
        expected: Expected::Crashes(CLASS_FP),
        code: user_float,
    },
    Program {
        name: "isolated",
        description: "keep its argument in memory across a sleep",
        expected: Expected::Exits(0),
        code: user_isolated,
    },
    Program {
        name: "overflow",
        description: "grow the stack until it hits the guard page",
        expected: Expected::Crashes(CLASS_DATA_ABORT_EL0),
        code: user_overflow,
    },
    Program {
        name: "writecode",
        description: "overwrite its own code",
        expected: Expected::Crashes(CLASS_DATA_ABORT_EL0),
        code: user_write_code,
    },
    Program {
        name: "runstack",
        description: "jump to the stack",
        expected: Expected::Crashes(CLASS_INSTRUCTION_ABORT_EL0),
        code: user_run_stack,
    },
];

/// The code of all built-in programs, as laid out in the kernel image.
pub fn image() -> &'static [u8] {
    let start = &raw const user_image_start;
    let len = &raw const user_image_end as usize - start as usize;
    unsafe { core::slice::from_raw_parts(start, len) }
}
