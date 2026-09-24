// process/mod.rs
//! User programs: tasks that run at EL0 and reach the kernel only through system calls
//! (`rustypi_abi`). A program that faults is killed and the kernel carries on.
//!
//! A program is a built-in test program or an ELF executable (`rustypi_core::elf`). Each
//! has its own address space (`rustypi_core::paging`) and ASID: its segments from
//! `USER_BASE`, and a stack just below the top of the window with its arguments on top, as
//! `rustypi_abi` lays out. Everything else is unmapped, so running off either end of the
//! stack faults.
//!
//! A system call runs like task code on the program's kernel stack, with interrupts enabled:
//! it can sleep, block or be preempted like any kernel task, and its registers wait in the
//! saved `ExceptionContext` until it returns.

mod programs;

pub use programs::{Program, PROGRAMS};

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use core::fmt;
use core::time::Duration;
use rustypi_abi::{encode_result, Errno, Registers, Syscall, MAX_WRITE, SVC_SYSCALL};
use rustypi_abi::layout::{MAX_ARGS, STACK_SIZE, STACK_TOP, USER_BASE};
use rustypi_core::elf;
use rustypi_core::paging::{Access, AddressSpace, PAGE_SIZE};
use rustypi_core::sched::TaskId;
use crate::arch::exception::{self, ExceptionContext, CLASS_SVC};
use crate::arch::{self, mmu};
use crate::drivers::timer;
use crate::sched::{self, UserStart};
use crate::synchronization::{interface::Mutex, IrqLock};
use crate::{print, println};

/// Where built-in program code goes.
const CODE_BASE: u64 = USER_BASE;

/// How a program ended.
#[derive(Clone, Copy, Debug)]
pub enum Exit {
    /// It called `exit` with this code.
    Code(i32),
    /// It faulted and was killed.
    Crashed(Fault),
    /// `kill` stopped it.
    Killed,
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
            Exit::Killed => write!(f, "killed"),
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
    /// The arguments are longer than `MAX_ARGS`.
    ArgsTooLong,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SpawnError::TooManyPrograms => write!(f, "too many programs running (at most {})", Asid::COUNT - 1),
            SpawnError::ArgsTooLong => write!(f, "arguments longer than {} bytes", MAX_ARGS),
        }
    }
}

/// What a program runs.
pub enum Code<'a> {
    /// One of the built-in test programs.
    Builtin(&'static Program),
    /// A validated ELF executable.
    Elf(&'a elf::Program<'a>),
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
    name: String,
    memory: AddressSpace,
    /// Held until the program is gone; only its release matters.
    _asid: Asid,
    /// Where its exit goes, for whoever waits on it.
    exit: Arc<IrqLock<Option<Exit>>>,
    /// Set by `kill`: it exits instead of returning from its current system call.
    killed: bool,
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

/// Starts a program called `name` in an address space of its own, with `args`.
pub fn spawn(name: &str, code: Code, args: &str) -> Result<Process, SpawnError> {
    if args.len() > MAX_ARGS {
        return Err(SpawnError::ArgsTooLong);
    }
    let asid = Asid::allocate().ok_or(SpawnError::TooManyPrograms)?;
    let mut memory = AddressSpace::new(mmu::kernel_table());

    // Nothing else is mapped yet, and ELF segments are checked to fit below the stack, so
    // none of the mapping can collide.
    let (entry, code_ranges) = match code {
        Code::Builtin(program) => {
            let code = programs::image();
            memory.map_range(CODE_BASE, code.len() as u64, Access::ReadExecute).expect("code fits");
            memory.load(CODE_BASE, code).expect("code pages are mapped");
            (CODE_BASE + program.offset() as u64, alloc::vec![(CODE_BASE, code.len() as u64)])
        }
        Code::Elf(program) => {
            program.load(&mut memory).expect("validated segments fit an empty address space");
            let code = program.segments.iter().filter(|segment| segment.access == Access::ReadExecute);
            (program.entry, code.map(|segment| (segment.address, segment.size)).collect())
        }
    };
    for (start, len) in code_ranges {
        for va in (start..start + len).step_by(PAGE_SIZE) {
            let (frame, _) = memory.translate(va).expect("code pages are mapped");
            mmu::sync_instruction_cache(frame as usize, PAGE_SIZE);
        }
    }

    // The arguments go at the top of the stack, which starts just below them.
    memory.map_range(STACK_TOP - STACK_SIZE, STACK_SIZE, Access::ReadWrite).expect("stack fits");
    let args_at = (STACK_TOP - args.len() as u64) & !15;
    memory.load(args_at, args.as_bytes()).expect("stack pages are mapped");

    let start = UserStart {
        translation_base: memory.translation_base(asid.0),
        entry,
        stack_top: args_at,
        args: [args_at, args.len() as u64],
    };
    let exit = Arc::new(IrqLock::new(None));
    let running = Running { name: name.into(), memory, _asid: asid, exit: exit.clone(), killed: false };
    // Registered under the lock, so the program can't make a system call before it is known.
    let id = PROCESSES.lock(|processes| {
        let id = sched::spawn_user(name, start);
        processes.insert(id, running);
        id
    });
    Ok(Process { id, exit })
}

#[derive(Debug)]
pub enum KillError {
    /// No program runs as that task: it is a kernel task, or gone.
    NotAProgram,
}

impl fmt::Display for KillError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            KillError::NotAProgram => write!(f, "not a running program"),
        }
    }
}

/// Stops the program running as task `id`. It may be running user code, or in a system call
/// with the kernel partway through something on its behalf, so it isn't torn down from here:
/// user code is sent straight to `exit`, and a system call exits instead of returning (right
/// away, if it was asleep).
pub fn kill(id: TaskId) -> Result<(), KillError> {
    PROCESSES.lock(|processes| {
        let running = processes.get_mut(&id).ok_or(KillError::NotAProgram)?;
        running.killed = true;
        if !sched::redirect_to_kernel(id, exit_killed) {
            sched::interrupt(id);
        }
        Ok(())
    })
}

/// Where a program killed while at EL0 resumes, on its kernel stack.
extern "C" fn exit_killed() -> ! {
    exit(Exit::Killed)
}

fn was_killed(id: TaskId) -> bool {
    PROCESSES.lock(|processes| processes.get(&id).is_some_and(|running| running.killed))
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
    if was_killed(sched::current()) {
        exit(Exit::Killed);
    }
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
