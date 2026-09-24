//! The runtime for RustyPI user programs: the entry point, system calls, printing and panics.
//!
//! A program is a `no_std`, `no_main` binary that names its main function with [`entry!`]:
//!
//! ```ignore
//! #![no_std]
//! #![no_main]
//!
//! use rustypi_user::{entry, println};
//!
//! entry!(main);
//!
//! fn main() {
//!     println!("hello, {}", rustypi_user::args());
//! }
//! ```
//!
//! Main can return `()`, an `i32` exit code, or a `Result` whose error is printed (exit code
//! 1). A panic prints its message and exits with code 101.
//!
//! There is a heap, which grows as needed: add `extern crate alloc;` to use `Vec`, `String`,
//! `Box` and the rest of `alloc`.

#![no_std]

pub mod fs;
pub mod heap;
pub mod io;
pub mod process;
pub mod random;
pub mod screen;
pub mod syscall;
pub mod time;

pub use rustypi_abi as abi;

use core::fmt;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use rustypi_abi::Syscall;

/// Names the program's main function. Use once, in the binary.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[no_mangle]
        fn __rustypi_main() -> i32 {
            $crate::Termination::exit_code($main())
        }
    };
}

/// What main may return.
pub trait Termination {
    fn exit_code(self) -> i32;
}

impl Termination for () {
    fn exit_code(self) -> i32 {
        0
    }
}

impl Termination for i32 {
    fn exit_code(self) -> i32 {
        self
    }
}

impl<T: Termination, E: fmt::Debug> Termination for Result<T, E> {
    fn exit_code(self) -> i32 {
        match self {
            Ok(value) => value.exit_code(),
            Err(error) => {
                println!("error: {:?}", error);
                1
            }
        }
    }
}

/// A handle from the kernel, closed when dropped.
pub(crate) struct Handle(pub(crate) u64);

impl Handle {
    /// Gives up the handle without closing it, for calls that close it themselves.
    pub(crate) fn into_raw(self) -> u64 {
        let handle = self.0;
        core::mem::forget(self);
        handle
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = syscall::call(Syscall::Close { handle: self.0 });
    }
}

static ARGS: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());
static ARGS_LEN: AtomicUsize = AtomicUsize::new(0);

/// Where the kernel starts the program: x0 and x1 hold its arguments.
#[doc(hidden)]
#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(args: *mut u8, len: usize) -> ! {
    ARGS.store(args, Ordering::Relaxed);
    ARGS_LEN.store(len, Ordering::Relaxed);
    extern "Rust" {
        fn __rustypi_main() -> i32;
    }
    let code = unsafe { __rustypi_main() };
    exit(code)
}

/// The rest of the command line the program was started with.
pub fn args() -> &'static str {
    let len = ARGS_LEN.load(Ordering::Relaxed);
    if len == 0 {
        return "";
    }
    // The kernel put them at the top of the stack, above anything the program uses.
    let bytes = unsafe { core::slice::from_raw_parts(ARGS.load(Ordering::Relaxed), len) };
    core::str::from_utf8(bytes).unwrap_or("")
}

/// Ends the program.
pub fn exit(code: i32) -> ! {
    let _ = syscall::call(Syscall::Exit { code });
    unreachable!("exit returned")
}

/// Lets other tasks run.
pub fn yield_now() {
    let _ = syscall::call(Syscall::Yield);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("{}", info);
    exit(101)
}
