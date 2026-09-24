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

/// Most bytes one `Random` fills.
pub const MAX_RANDOM: usize = 256;

/// Longest path `Open` and `OpenDir` take, in bytes.
pub const MAX_PATH: usize = 256;

/// Biggest file `Open` takes, in bytes: the kernel reads it whole.
pub const MAX_FILE: usize = 4 * 1024 * 1024;

/// Most handles a program can have open at once (files, directories and children), besides
/// `INPUT`.
pub const MAX_HANDLES: usize = 16;

/// The handle every program starts with: what the user types while it runs in the
/// foreground, a line at a time, each ending with `\n`.
pub const INPUT: u64 = 0;

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
    Random = 7,
    Open = 8,
    OpenDir = 9,
    ReadDir = 10,
    Close = 11,
    Spawn = 12,
    Wait = 13,
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
            7 => Number::Random,
            8 => Number::Open,
            9 => Number::OpenDir,
            10 => Number::ReadDir,
            11 => Number::Close,
            12 => Number::Spawn,
            13 => Number::Wait,
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
    /// Reads up to `len` bytes (at most `MAX_READ`) from a handle into `ptr`. Returns how
    /// many were read: 0 at the end of a file. Reading `INPUT` waits until there is some.
    Read { handle: u64, ptr: u64, len: u64 },
    /// Fills up to `len` bytes (at most `MAX_RANDOM`) at `ptr` with random bytes from the
    /// hardware generator. Returns how many were filled.
    Random { ptr: u64, len: u64 },
    /// Opens the file at the UTF-8 path `path..path + len` for reading. Returns a handle.
    /// Paths start at the root of the SD card, with or without a leading `/`.
    Open { path: u64, len: u64 },
    /// Opens a directory, to list with `ReadDir`. Returns a handle.
    OpenDir { path: u64, len: u64 },
    /// Writes the directory's next [`DirEntry`] to `entry`. Returns 1, or 0 once all have
    /// been read.
    ReadDir { handle: u64, entry: u64 },
    /// Closes a handle from `Open`, `OpenDir` or `Spawn` (the child runs on, unwatched).
    /// Exiting closes them all.
    Close { handle: u64 },
    /// Starts the program in the file at `path..path + path_len`, with the arguments at
    /// `args..args + args_len`. Returns a handle for `Wait`. While this program waits on it,
    /// input sent to this program goes to the child.
    Spawn { path: u64, path_len: u64, args: u64, args_len: u64 },
    /// Waits for a child from `Spawn` to end, and closes its handle. Returns its
    /// [`ExitStatus`], encoded.
    Wait { handle: u64 },
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
            Syscall::Random { .. } => Number::Random,
            Syscall::Open { .. } => Number::Open,
            Syscall::OpenDir { .. } => Number::OpenDir,
            Syscall::ReadDir { .. } => Number::ReadDir,
            Syscall::Close { .. } => Number::Close,
            Syscall::Spawn { .. } => Number::Spawn,
            Syscall::Wait { .. } => Number::Wait,
        }
    }

    pub const fn encode(&self) -> Registers {
        let args = match *self {
            // Sign-extended, like any i32 in a 64-bit register.
            Syscall::Exit { code } => [code as i64 as u64, 0, 0, 0, 0, 0],
            Syscall::Write { ptr, len } | Syscall::Random { ptr, len } => [ptr, len, 0, 0, 0, 0],
            Syscall::Read { handle, ptr, len } => [handle, ptr, len, 0, 0, 0],
            Syscall::Open { path, len } | Syscall::OpenDir { path, len } => [path, len, 0, 0, 0, 0],
            Syscall::ReadDir { handle, entry } => [handle, entry, 0, 0, 0, 0],
            Syscall::Close { handle } | Syscall::Wait { handle } => [handle, 0, 0, 0, 0, 0],
            Syscall::Spawn { path, path_len, args, args_len } => [path, path_len, args, args_len, 0, 0],
            Syscall::Sleep { micros } => [micros, 0, 0, 0, 0, 0],
            Syscall::Map { len } => [len, 0, 0, 0, 0, 0],
            Syscall::Yield | Syscall::Uptime => [0; 6],
        };
        Registers { number: self.number() as u64, args }
    }

    /// Unknown numbers are `NoSys`; arguments out of range for their type are `Invalid`.
    /// Arguments a call doesn't take are ignored.
    pub fn decode(registers: Registers) -> Result<Syscall, Errno> {
        let [a0, a1, a2, a3, ..] = registers.args;
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
            Number::Read => Syscall::Read { handle: a0, ptr: a1, len: a2 },
            Number::Random => Syscall::Random { ptr: a0, len: a1 },
            Number::Open => Syscall::Open { path: a0, len: a1 },
            Number::OpenDir => Syscall::OpenDir { path: a0, len: a1 },
            Number::ReadDir => Syscall::ReadDir { handle: a0, entry: a1 },
            Number::Close => Syscall::Close { handle: a0 },
            Number::Spawn => Syscall::Spawn { path: a0, path_len: a1, args: a2, args_len: a3 },
            Number::Wait => Syscall::Wait { handle: a0 },
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
    /// No file or directory has that path.
    NotFound,
    /// It's a directory, where a file was wanted.
    IsADirectory,
    /// It's not a directory, where one was wanted.
    NotADirectory,
    /// Not an open handle, or the wrong kind for the call.
    BadHandle,
    /// Too many open already (handles, or programs).
    TooMany,
    /// The SD card or its filesystem failed.
    Io,
    /// Not a program RustyPI can run.
    NotExecutable,
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
            Errno::NotFound => 5,
            Errno::IsADirectory => 6,
            Errno::NotADirectory => 7,
            Errno::BadHandle => 8,
            Errno::TooMany => 9,
            Errno::Io => 10,
            Errno::NotExecutable => 11,
            Errno::Unknown => 4095,
        }
    }

    const fn from_code(code: u64) -> Errno {
        match code {
            1 => Errno::NoSys,
            2 => Errno::Invalid,
            3 => Errno::Fault,
            4 => Errno::NoMemory,
            5 => Errno::NotFound,
            6 => Errno::IsADirectory,
            7 => Errno::NotADirectory,
            8 => Errno::BadHandle,
            9 => Errno::TooMany,
            10 => Errno::Io,
            11 => Errno::NotExecutable,
            _ => Errno::Unknown,
        }
    }
}

/// How a program ended, as `Wait` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitStatus {
    /// It exited with this code.
    Code(i32),
    /// It faulted, and the kernel killed it.
    Crashed,
    /// It was killed.
    Killed,
}

const STATUS_CODE: u64 = 0;
const STATUS_CRASHED: u64 = 1;
const STATUS_KILLED: u64 = 2;

impl ExitStatus {
    /// Into a system call result: the kind above bit 32, a code's bits below.
    pub const fn encode(self) -> u64 {
        match self {
            ExitStatus::Code(code) => STATUS_CODE << 32 | code as u32 as u64,
            ExitStatus::Crashed => STATUS_CRASHED << 32,
            ExitStatus::Killed => STATUS_KILLED << 32,
        }
    }

    pub const fn decode(value: u64) -> Option<ExitStatus> {
        match value >> 32 {
            STATUS_CODE => Some(ExitStatus::Code(value as u32 as i32)),
            STATUS_CRASHED if value as u32 == 0 => Some(ExitStatus::Crashed),
            STATUS_KILLED if value as u32 == 0 => Some(ExitStatus::Killed),
            _ => None,
        }
    }
}

/// One directory entry, as `ReadDir` writes it into program memory. Laid out without padding,
/// so it is plain bytes on both sides.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// Bytes; 0 for directories.
    pub size: u64,
    name_len: u16,
    kind: u8,
    _reserved: [u8; 5],
    name: [u8; MAX_NAME],
}

/// Longest name a `DirEntry` holds, in bytes. Longer names are cut at a character boundary.
pub const MAX_NAME: usize = 248;

const _: () = assert!(size_of::<DirEntry>() == 8 + 2 + 1 + 5 + MAX_NAME, "DirEntry has padding");

const KIND_FILE: u8 = 0;
const KIND_DIRECTORY: u8 = 1;

impl DirEntry {
    pub const EMPTY: DirEntry = DirEntry { size: 0, name_len: 0, kind: KIND_FILE, _reserved: [0; 5], name: [0; MAX_NAME] };

    pub fn new(name: &str, is_dir: bool, size: u64) -> Self {
        let mut len = name.len().min(MAX_NAME);
        while !name.is_char_boundary(len) {
            len -= 1;
        }
        let mut entry = DirEntry { size, name_len: len as u16, kind: if is_dir { KIND_DIRECTORY } else { KIND_FILE }, ..DirEntry::EMPTY };
        entry.name[..len].copy_from_slice(&name.as_bytes()[..len]);
        entry
    }

    /// The name, or `""` if the bytes (from a buggy kernel) aren't valid.
    pub fn name(&self) -> &str {
        let len = (self.name_len as usize).min(MAX_NAME);
        core::str::from_utf8(&self.name[..len]).unwrap_or("")
    }

    pub fn is_dir(&self) -> bool {
        self.kind == KIND_DIRECTORY
    }

    /// Its bytes, as the kernel copies them into program memory.
    pub fn as_bytes(&self) -> &[u8] {
        // repr(C) with no padding (checked above): every byte is initialized.
        unsafe { core::slice::from_raw_parts((self as *const DirEntry).cast(), size_of::<DirEntry>()) }
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

    const ALL: [Syscall; 15] = [
        Syscall::Exit { code: 0 },
        Syscall::Exit { code: -7 },
        Syscall::Write { ptr: 0x8000_0000, len: 12 },
        Syscall::Yield,
        Syscall::Sleep { micros: 1_500 },
        Syscall::Uptime,
        Syscall::Map { len: 8192 },
        Syscall::Read { handle: 3, ptr: 0x8000_1000, len: 64 },
        Syscall::Random { ptr: 0x8000_2000, len: 16 },
        Syscall::Open { path: 0x8000_3000, len: 9 },
        Syscall::OpenDir { path: 0x8000_3000, len: 4 },
        Syscall::ReadDir { handle: 2, entry: 0x8000_4000 },
        Syscall::Close { handle: 2 },
        Syscall::Spawn { path: 0x8000_3000, path_len: 9, args: 0x8000_3100, args_len: 3 },
        Syscall::Wait { handle: 4 },
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
        let errors = [
            Errno::NoSys,
            Errno::Invalid,
            Errno::Fault,
            Errno::NoMemory,
            Errno::NotFound,
            Errno::IsADirectory,
            Errno::NotADirectory,
            Errno::BadHandle,
            Errno::TooMany,
            Errno::Io,
            Errno::NotExecutable,
        ]
        .map(Err);
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
    fn exit_statuses_survive_encoding_as_results() {
        for status in [ExitStatus::Code(0), ExitStatus::Code(-1), ExitStatus::Code(i32::MAX), ExitStatus::Crashed, ExitStatus::Killed] {
            assert_eq!(decode_result(encode_result(Ok(status.encode()))).map(ExitStatus::decode), Ok(Some(status)));
        }
        assert_eq!(ExitStatus::decode(3 << 32), None);
        assert_eq!(ExitStatus::decode(STATUS_CRASHED << 32 | 5), None);
    }

    #[test]
    fn dir_entries_keep_their_fields() {
        let entry = DirEntry::new("kernel8.img", false, 201_832);
        assert_eq!((entry.name(), entry.is_dir(), entry.size), ("kernel8.img", false, 201_832));
        assert!(DirEntry::new("bin", true, 0).is_dir());
        assert_eq!(entry.as_bytes().len(), size_of::<DirEntry>());
    }

    #[test]
    fn long_names_are_cut_at_a_character_boundary() {
        let long = "é".repeat(200); // 400 bytes
        let entry = DirEntry::new(&long, false, 0);
        assert_eq!(entry.name(), "é".repeat(MAX_NAME / 2));
        // Here the cut would fall in the middle of an "é".
        let odd = format!("x{}", "é".repeat(200));
        assert_eq!(DirEntry::new(&odd, false, 0).name().len(), MAX_NAME - 1);
    }

    #[test]
    #[should_panic]
    fn oversized_results_are_a_kernel_bug() {
        encode_result(Ok(MAX_RESULT + 1));
    }
}
