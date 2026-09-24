//! The system call interface between the RustyPI kernel and its user programs.
//!
//! A program asks for a system call with `svc #0` ([`SVC_SYSCALL`]), the call's
//! [`Number`] in x8 and up to six arguments in x0 to x5. The result comes back in x0,
//! encoded by [`encode_result`]. Other registers are preserved.
//!
//! Both sides go through the typed [`Syscall`]: programs `encode` one into registers and
//! the kernel `decode`s the registers back, so neither handles raw numbers. Arguments are
//! always integers.
//!
//! A program starts at its ELF entry point with x0 pointing at its arguments (the rest of
//! the command line, UTF-8) and x1 holding their length in bytes. They lie at the top of its
//! stack, which the stack pointer starts just below.
//!
//! No dependencies and no allocation, so user programs can use it without a heap.

#![cfg_attr(not(test), no_std)]

/// Where things are in a program's address space.
pub mod layout {
    pub const PAGE_SIZE: u64 = 4096;
    /// All user memory lies in `USER_BASE..USER_END`. Programs are linked to start here.
    pub const USER_BASE: u64 = 0x8000_0000;
    pub const USER_END: u64 = 0xC000_0000;
    /// The stack ends a page below the top of the user window.
    pub const STACK_TOP: u64 = USER_END - PAGE_SIZE;
    pub const STACK_SIZE: u64 = 64 * 1024;
    /// A program's segments must end by here, leaving an unmapped page below the stack.
    pub const PROGRAM_END: u64 = STACK_TOP - STACK_SIZE - PAGE_SIZE;
    /// Most bytes of arguments a program is started with.
    pub const MAX_ARGS: usize = 1024;
}

/// The `svc` immediate for a system call.
pub const SVC_SYSCALL: u16 = 0;

/// Most bytes one `Write` takes. Longer writes are cut short, and return how much was written.
pub const MAX_WRITE: usize = 256;

/// Most bytes one `Read` returns.
pub const MAX_READ: usize = 256;

/// Identifies a system call, in x8.
#[repr(u64)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Number {
    Exit = 0,
    Write = 1,
    Yield = 2,
    Sleep = 3,
    Uptime = 4,
    Map = 5,
    Read = 6,
}

impl Number {
    pub const fn from_raw(raw: u64) -> Option<Number> {
        Some(match raw {
            0 => Number::Exit,
            1 => Number::Write,
            2 => Number::Yield,
            3 => Number::Sleep,
            4 => Number::Uptime,
            5 => Number::Map,
            6 => Number::Read,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Syscall {
    /// Ends the program with an exit code. Never returns.
    Exit { code: i32 },
    /// Writes up to `MAX_WRITE` bytes from `ptr` to the console. Returns how many were
    /// written. Invalid UTF-8 and `$` are shown as `?`.
    Write { ptr: u64, len: u64 },
    /// Lets other tasks run. Returns 0.
    Yield,
    /// Sleeps for at least `micros` microseconds. Returns 0.
    Sleep { micros: u64 },
    /// Returns the microseconds since the board was reset.
    Uptime,
    /// Grows the program's heap by `len` bytes (rounded up to whole pages) of zeroed,
    /// readable and writable memory. Returns where the new memory starts; each call's memory
    /// follows straight on from the last. With `len` 0, returns where the next would start.
    /// `NoMemory` if the heap can't grow that far.
    Map { len: u64 },
    /// Reads up to `len` bytes (at most `MAX_READ`) of input into `ptr`, waiting until there
    /// is some. Returns how many were read. Input is what the user types while the program
    /// runs in the foreground, a line at a time, each ending with `\n`.
    Read { ptr: u64, len: u64 },
}

/// The registers a system call is made with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Registers {
    /// x8.
    pub number: u64,
    /// x0 to x5. Arguments a call doesn't take are zero.
    pub args: [u64; 6],
}

impl Syscall {
    pub const fn number(&self) -> Number {
        match self {
            Syscall::Exit { .. } => Number::Exit,
            Syscall::Write { .. } => Number::Write,
            Syscall::Yield => Number::Yield,
            Syscall::Sleep { .. } => Number::Sleep,
            Syscall::Uptime => Number::Uptime,
            Syscall::Map { .. } => Number::Map,
            Syscall::Read { .. } => Number::Read,
        }
    }

    pub const fn encode(&self) -> Registers {
        let args = match *self {
            // Sign-extended, like any i32 in a 64-bit register.
            Syscall::Exit { code } => [code as i64 as u64, 0, 0, 0, 0, 0],
            Syscall::Write { ptr, len } | Syscall::Read { ptr, len } => [ptr, len, 0, 0, 0, 0],
            Syscall::Sleep { micros } => [micros, 0, 0, 0, 0, 0],
            Syscall::Map { len } => [len, 0, 0, 0, 0, 0],
            Syscall::Yield | Syscall::Uptime => [0; 6],
        };
        Registers { number: self.number() as u64, args }
    }

    /// Unknown numbers are `NoSys`; arguments out of range for their type are `Invalid`.
    /// Arguments a call doesn't take are ignored.
    pub fn decode(registers: Registers) -> Result<Syscall, Errno> {
        let [a0, a1, ..] = registers.args;
        let number = Number::from_raw(registers.number).ok_or(Errno::NoSys)?;
        Ok(match number {
            Number::Exit => {
                let code = i32::try_from(a0 as i64).map_err(|_| Errno::Invalid)?;
                Syscall::Exit { code }
            }
            Number::Write => Syscall::Write { ptr: a0, len: a1 },
            Number::Yield => Syscall::Yield,
            Number::Sleep => Syscall::Sleep { micros: a0 },
            Number::Uptime => Syscall::Uptime,
            Number::Map => Syscall::Map { len: a0 },
            Number::Read => Syscall::Read { ptr: a0, len: a1 },
        })
    }
}

/// Why a system call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Errno {
    /// No system call has that number.
    NoSys,
    /// An argument is out of range.
    Invalid,
    /// A pointer argument points outside the program's memory.
    Fault,
    /// There isn't enough memory, or room in the address space.
    NoMemory,
    /// A code this version of the ABI doesn't know, from a newer kernel.
    Unknown,
}

impl Errno {
    /// The positive code; x0 holds its negation.
    pub const fn code(self) -> u64 {
        match self {
            Errno::NoSys => 1,
            Errno::Invalid => 2,
            Errno::Fault => 3,
            Errno::NoMemory => 4,
            Errno::Unknown => 4095,
        }
    }

    const fn from_code(code: u64) -> Errno {
        match code {
            1 => Errno::NoSys,
            2 => Errno::Invalid,
            3 => Errno::Fault,
            4 => Errno::NoMemory,
            _ => Errno::Unknown,
        }
    }
}

/// Largest successful result: values with the top bit set would read as errors.
pub const MAX_RESULT: u64 = i64::MAX as u64;

/// Packs a result into x0: a value from 0 to `MAX_RESULT`, or a negated error code.
pub const fn encode_result(result: Result<u64, Errno>) -> u64 {
    match result {
        Ok(value) => {
            assert!(value <= MAX_RESULT, "system call result doesn't fit");
            value
        }
        Err(errno) => (errno.code() as i64).wrapping_neg() as u64,
    }
}

pub const fn decode_result(x0: u64) -> Result<u64, Errno> {
    if x0 <= MAX_RESULT {
        Ok(x0)
    } else {
        Err(Errno::from_code((x0 as i64).wrapping_neg() as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Syscall; 8] = [
        Syscall::Exit { code: 0 },
        Syscall::Exit { code: -7 },
        Syscall::Write { ptr: 0x8000_0000, len: 12 },
        Syscall::Yield,
        Syscall::Sleep { micros: 1_500 },
        Syscall::Uptime,
        Syscall::Map { len: 8192 },
        Syscall::Read { ptr: 0x8000_1000, len: 64 },
    ];

    #[test]
    fn every_call_survives_encoding() {
        for call in ALL {
            assert_eq!(Syscall::decode(call.encode()), Ok(call));
        }
    }

    #[test]
    fn numbers_survive_their_raw_form() {
        for call in ALL {
            assert_eq!(Number::from_raw(call.number() as u64), Some(call.number()));
        }
    }

    #[test]
    fn unused_argument_registers_are_zero_and_ignored() {
        assert_eq!(Syscall::Yield.encode().args, [0; 6]);
        let mut registers = Syscall::Sleep { micros: 5 }.encode();
        registers.args[3] = 0xDEAD;
        assert_eq!(Syscall::decode(registers), Ok(Syscall::Sleep { micros: 5 }));
    }

    #[test]
    fn exit_codes_are_sign_extended() {
        assert_eq!(Syscall::Exit { code: -1 }.encode().args[0], u64::MAX);
    }

    #[test]
    fn unknown_numbers_and_bad_arguments_are_rejected() {
        assert_eq!(Syscall::decode(Registers { number: 999, args: [0; 6] }), Err(Errno::NoSys));
        // 2^32 isn't an i32, and neither is a zero-extended -1.
        let exit = |a0| Registers { number: Number::Exit as u64, args: [a0, 0, 0, 0, 0, 0] };
        assert_eq!(Syscall::decode(exit(1 << 32)), Err(Errno::Invalid));
        assert_eq!(Syscall::decode(exit(0xFFFF_FFFF)), Err(Errno::Invalid));
    }

    #[test]
    fn results_survive_encoding() {
        let errors = [Errno::NoSys, Errno::Invalid, Errno::Fault, Errno::NoMemory].map(Err);
        for result in [Ok(0), Ok(42), Ok(MAX_RESULT)].into_iter().chain(errors) {
            assert_eq!(decode_result(encode_result(result)), result);
        }
        assert_eq!(encode_result(Err(Errno::Fault)), -3i64 as u64);
    }

    #[test]
    fn unknown_error_codes_decode_as_unknown() {
        assert_eq!(decode_result(-77i64 as u64), Err(Errno::Unknown));
        assert_eq!(decode_result(u64::MAX - MAX_RESULT), Err(Errno::Unknown)); // i64::MIN
    }

    #[test]
    #[should_panic]
    fn oversized_results_are_a_kernel_bug() {
        encode_result(Ok(MAX_RESULT + 1));
    }
}
