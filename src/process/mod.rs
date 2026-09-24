// process/mod.rs
//! User programs: tasks that run at EL0 and reach the kernel only through system calls
//! (`rustypi_abi`). A program that faults is killed and the kernel carries on.
//!
//! For now every program shares the one user window (`mmu::USER_BASE`): the built-in
//! programs' code is copied to its start by `init`, and each running program gets one of the
//! stack slots in its second half.
//!
//! A system call runs like task code on the program's kernel stack, with interrupts enabled:
//! it can sleep, block or be preempted like any kernel task, and its registers wait in the
//! saved `ExceptionContext` until it returns.

mod programs;

pub use programs::{Program, PROGRAMS};

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::fmt;
use core::time::Duration;
use rustypi_abi::{encode_result, Errno, Registers, Syscall, MAX_WRITE, SVC_SYSCALL};
use rustypi_core::sched::TaskId;
use rustypi_core::user::Region;
use crate::arch::exception::{self, ExceptionContext, CLASS_SVC};
use crate::arch::{self, mmu};
use crate::drivers::timer;
use crate::synchronization::{interface::Mutex, IrqLock};
use crate::{print, println, sched};

/// The memory programs may hand to system calls.
const USER_MEMORY: Region = Region::new(mmu::USER_BASE as u64, mmu::USER_SIZE as u64);

/// Program code fills the first half of the user window, stacks the second.
const CODE_SIZE: usize = mmu::USER_SIZE / 2;
const STACK_SLOTS: usize = 16;
const USER_STACK_SIZE: usize = (mmu::USER_SIZE - CODE_SIZE) / STACK_SLOTS;

/// How a program ended.
#[derive(Clone, Copy, Debug)]
pub enum Exit {
    /// It called `exit` with this code.
    Code(i32),
    /// It faulted and was killed.
    Crashed(Fault),
}

/// A synchronous exception a program caused.
#[derive(Clone, Copy, Debug)]
pub struct Fault {
    pub esr: u64,
    /// The faulting instruction.
    pub pc: u64,
    /// The address it was accessing, for memory faults.
    pub far: u64,
}

impl Fault {
    /// The exception class in ESR_EL1 bits [31:26].
    pub fn class(&self) -> u64 {
        self.esr >> 26
    }
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Exit::Code(code) => write!(f, "exited {}", code),
            Exit::Crashed(fault) => write!(
                f,
                "crashed: {} at {:#x} (far {:#x})",
                exception::class_name(fault.esr),
                fault.pc,
                fault.far,
            ),
        }
    }
}

#[derive(Debug)]
pub enum SpawnError {
    /// Every user stack slot is taken.
    TooManyPrograms,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SpawnError::TooManyPrograms => write!(f, "too many programs running (at most {})", STACK_SLOTS),
        }
    }
}

/// What the kernel keeps about a running program.
struct Record {
    name: &'static str,
    stack_slot: usize,
    /// Set just before its task finishes.
    exit: IrqLock<Option<Exit>>,
}

/// Running programs, by task.
static PROCESSES: IrqLock<BTreeMap<TaskId, Arc<Record>>> = IrqLock::new(BTreeMap::new());
/// Bit `n` is set while stack slot `n` is in use.
static STACKS: IrqLock<u32> = IrqLock::new(0);

/// A started program.
pub struct Process {
    id: TaskId,
    record: Arc<Record>,
}

impl Process {
    pub fn id(&self) -> TaskId {
        self.id
    }

    /// Waits for the program to end.
    pub fn wait(&self) -> Exit {
        sched::join(self.id);
        self.record.exit.lock(|exit| exit.expect("a finished program has an exit"))
    }
}

/// Copies the built-in programs into the user window. Call once the MMU is on.
pub fn init() {
    let image = programs::image();
    assert!(image.len() <= CODE_SIZE, "built-in programs don't fit the user window");
    unsafe { core::ptr::copy_nonoverlapping(image.as_ptr(), mmu::USER_BASE as *mut u8, image.len()) };
    mmu::sync_instruction_cache(mmu::USER_BASE, image.len());
}

/// Starts a built-in program.
pub fn spawn(program: &Program) -> Result<Process, SpawnError> {
    let slot = STACKS
        .lock(|used| {
            let free = used.trailing_ones() as usize;
            (free < STACK_SLOTS).then(|| {
                *used |= 1 << free;
                free
            })
        })
        .ok_or(SpawnError::TooManyPrograms)?;
    let bottom = mmu::USER_BASE + CODE_SIZE + slot * USER_STACK_SIZE;
    // Don't let a program read what the last one left on the stack.
    unsafe { core::ptr::write_bytes(bottom as *mut u8, 0, USER_STACK_SIZE) };

    let record = Arc::new(Record { name: program.name, stack_slot: slot, exit: IrqLock::new(None) });
    // Registered under the lock, so the program can't make a system call before it is known.
    let id = PROCESSES.lock(|processes| {
        let id = sched::spawn_user(program.name, program.entry(), bottom + USER_STACK_SIZE);
        processes.insert(id, record.clone());
        id
    });
    Ok(Process { id, record })
}

/// Handles a synchronous exception from EL0: a system call, or a fault that kills the program.
pub fn on_user_sync(ctx: *mut ExceptionContext) -> *mut ExceptionContext {
    let context = unsafe { &mut *ctx };
    if context.class() != CLASS_SVC || context.esr & 0xFFFF != SVC_SYSCALL as u64 {
        let fault = Fault { esr: context.esr, pc: context.elr, far: exception::far() };
        arch::irq_enable();
        exit(Exit::Crashed(fault));
    }

    let [x0, x1, x2, x3, x4, x5, ..] = context.gpr;
    let registers = Registers { number: context.gpr[8], args: [x0, x1, x2, x3, x4, x5] };
    arch::irq_enable();
    let result = Syscall::decode(registers).and_then(syscall);
    // exception_restore loads ELR and SPSR from the context before the eret; an interrupt in
    // between would overwrite them.
    arch::irq_disable();
    context.gpr[0] = encode_result(result);
    ctx
}

fn syscall(call: Syscall) -> Result<u64, Errno> {
    match call {
        Syscall::Exit { code } => exit(Exit::Code(code)),
        Syscall::Write { ptr, len } => write(ptr, len),
        Syscall::Yield => {
            sched::yield_now();
            Ok(0)
        }
        Syscall::Sleep { micros } => {
            sched::sleep(Duration::from_micros(micros));
            Ok(0)
        }
        Syscall::Uptime => Ok(timer::now_us()),
    }
}

fn write(ptr: u64, len: u64) -> Result<u64, Errno> {
    let mut buf = [0; MAX_WRITE];
    let len = len.min(MAX_WRITE as u64) as usize;
    let bytes = copy_from_user(ptr, &mut buf[..len])?;
    print!("{}", PlainText(bytes));
    Ok(len as u64)
}

/// Copies program memory at `addr` into `buf`, if it is all program memory. Other programs
/// share the window and could change it meanwhile, so the kernel only works on the copy.
fn copy_from_user(addr: u64, buf: &mut [u8]) -> Result<&[u8], Errno> {
    if !USER_MEMORY.contains(addr, buf.len() as u64) {
        return Err(Errno::Fault);
    }
    // The whole window is mapped, so the checked range can't fault.
    unsafe { core::ptr::copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), buf.len()) };
    Ok(buf)
}

/// Program output, made safe for the console and the Arduino link.
struct PlainText<'a>(&'a [u8]);

impl fmt::Display for PlainText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut result = Ok(());
        rustypi_core::link::plain_text(self.0, |text| {
            if result.is_ok() {
                result = f.write_str(text);
            }
        });
        result
    }
}

/// Ends the running program. Called on its kernel stack, with IRQs enabled.
fn exit(status: Exit) -> ! {
    let id = sched::current();
    let record = PROCESSES.lock(|processes| processes.remove(&id)).expect("a user task has a process");
    record.exit.lock(|exit| *exit = Some(status));
    STACKS.lock(|used| *used &= !(1 << record.stack_slot));
    println!("[{}] {} {}", id.0, record.name, status);
    // sched::exit never returns, so nothing would drop it.
    drop(record);
    sched::exit()
}
