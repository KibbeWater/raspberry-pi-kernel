//! Making system calls.

use core::arch::asm;
use rustypi_abi::{decode_result, Errno, Registers, Syscall, SVC_SYSCALL};

/// Makes a system call and returns its result.
pub fn call(call: Syscall) -> Result<u64, Errno> {
    let Registers { number, args } = call.encode();
    let x0: u64;
    unsafe {
        asm!(
            "svc #{svc}",
            svc = const SVC_SYSCALL,
            inlateout("x0") args[0] => x0,
            in("x1") args[1],
            in("x2") args[2],
            in("x3") args[3],
            in("x4") args[4],
            in("x5") args[5],
            in("x8") number,
            options(nostack),
        );
    }
    decode_result(x0)
}
