// process/mod.rs
//! User programs: tasks that run at EL0 and reach the kernel only through system calls
//! (`rustypi_abi`). A program that faults is killed and the kernel carries on.
//!
//! Each program has its own address space (`rustypi_core::paging`) and ASID: its code,
//! read-only and executable, at the start of the user window, and a stack just below the
//! top. Everything else is unmapped, so running off either end of the stack faults.
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
use rustypi_core::paging::{Access, AddressSpace, PAGE_SIZE, USER_BASE, USER_END};
use rustypi_core::sched::TaskId;
use crate::arch::exception::{self, ExceptionContext, CLASS_SVC};
use crate::arch::{self, mmu};
use crate::drivers::timer;
use crate::sched::{self, UserStart};
use crate::synchronization::{interface::Mutex, IrqLock};
use crate::{print, println};

/// Where program code goes.
const CODE_BASE: u64 = USER_BASE;
/// The stack ends a page below the top of the window, with unmapped pages on either side.
const STACK_TOP: u64 = USER_END - PAGE_SIZE as u64;
const STACK_SIZE: u64 = 64 * 1024;

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
    /// Every ASID is taken.
    TooManyPrograms,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SpawnError::TooManyPrograms => write!(f, "too many programs running (at most {})", Asid::COUNT - 1),
        }
    }
}

/// An address space identifier, which tags the program's TLB entries. Given back when
/// dropped, which must come after flushing the TLB.
struct Asid(u8);

/// Bit `n` is set while ASID `n` is in use. ASID 0 is the kernel's.
static ASIDS: IrqLock<[u64; Asid::COUNT / 64]> = IrqLock::new([1, 0, 0, 0]);

impl Asid {
    /// With 8-bit ASIDs (TCR_EL1.AS = 0).
    const COUNT: usize = 256;

    fn allocate() -> Option<Asid> {
        ASIDS.lock(|used| {
            let n = (0..Asid::COUNT).find(|&n| used[n / 64] & 1 << (n % 64) == 0)?;
            used[n / 64] |= 1 << (n % 64);
            Some(Asid(n as u8))
        })
    }
}

impl Drop for Asid {
    fn drop(&mut self) {
        let n = self.0 as usize;
        ASIDS.lock(|used| used[n / 64] &= !(1 << (n % 64)));
    }
}

/// What the kernel keeps about a running program.
struct Running {
    name: &'static str,
    memory: AddressSpace,
    /// Held until the program is gone; only its release matters.
    _asid: Asid,
    /// Where its exit goes, for whoever waits on it.
    exit: Arc<IrqLock<Option<Exit>>>,
}

/// Running programs, by task.
static PROCESSES: IrqLock<BTreeMap<TaskId, Running>> = IrqLock::new(BTreeMap::new());

/// A started program.
pub struct Process {
    id: TaskId,
    exit: Arc<IrqLock<Option<Exit>>>,
}

impl Process {
    pub fn id(&self) -> TaskId {
        self.id
    }

    /// Waits for the program to end.
    pub fn wait(&self) -> Exit {
        sched::join(self.id);
        self.exit.lock(|exit| exit.expect("a finished program has an exit"))
    }
}

/// Starts a built-in program in an address space of its own, with `arg` in x0.
pub fn spawn(program: &Program, arg: u64) -> Result<Process, SpawnError> {
    let asid = Asid::allocate().ok_or(SpawnError::TooManyPrograms)?;
    let mut memory = AddressSpace::new(mmu::kernel_table());

    let code = programs::image();
    memory.map_range(CODE_BASE, code.len() as u64, Access::ReadExecute).expect("code fits the window");
    memory.load(CODE_BASE, code).expect("code pages are mapped");
    for va in (CODE_BASE..CODE_BASE + code.len() as u64).step_by(PAGE_SIZE) {
        let (frame, _) = memory.translate(va).expect("code pages are mapped");
        mmu::sync_instruction_cache(frame as usize, PAGE_SIZE);
    }
    memory.map_range(STACK_TOP - STACK_SIZE, STACK_SIZE, Access::ReadWrite).expect("stack fits the window");

    let start = UserStart {
        translation_base: memory.translation_base(asid.0),
        entry: CODE_BASE + program.offset() as u64,
        stack_top: STACK_TOP,
        arg,
    };
    let exit = Arc::new(IrqLock::new(None));
    let running = Running { name: program.name, memory, _asid: asid, exit: exit.clone() };
    // Registered under the lock, so the program can't make a system call before it is known.
    let id = PROCESSES.lock(|processes| {
        let id = sched::spawn_user(program.name, start);
        processes.insert(id, running);
        id
    });
    Ok(Process { id, exit })
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

/// Copies the running program's memory at `addr` into `buf`, if the program could read all of
/// it. Goes through its page tables rather than its pointer, so a bad address is an error
/// here instead of a fault in the kernel.
fn copy_from_user(addr: u64, buf: &mut [u8]) -> Result<&[u8], Errno> {
    let id = sched::current();
    PROCESSES.lock(|processes| {
        let running = processes.get(&id).expect("a user task has a process");
        running.memory.read_user(addr, buf).map_err(|_| Errno::Fault)
    })?;
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
    let running = PROCESSES.lock(|processes| processes.remove(&id)).expect("a user task has a process");
    // Off its page tables before they are freed, and nothing cached may still point into them
    // (or be tagged with its ASID, which goes back to the pool).
    sched::leave_user_space();
    mmu::flush_tlb();
    running.exit.lock(|exit| *exit = Some(status));
    println!("[{}] {} {}", id.0, running.name, status);
    // sched::exit never returns, so nothing would drop it.
    drop(running);
    sched::exit()
}
