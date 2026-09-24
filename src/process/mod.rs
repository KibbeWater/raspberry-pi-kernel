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

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use core::fmt;
use core::time::Duration;
use rustypi_abi::{encode_result, Errno, Registers, Syscall, MAX_RANDOM, MAX_READ, MAX_WRITE, SVC_SYSCALL};
use rustypi_abi::layout::{MAX_ARGS, PROGRAM_END, STACK_SIZE, STACK_TOP, USER_BASE};
use rustypi_core::elf;
use rustypi_core::paging::{Access, AddressSpace, MapError, PAGE_SIZE};
use rustypi_core::sched::TaskId;
use crate::arch::exception::{self, ExceptionContext, CLASS_SVC};
use crate::arch::{self, mmu};
use crate::drivers::timer;
use crate::sched::{self, UserStart};
use crate::synchronization::{interface::Mutex, IrqLock};
use crate::{print, println, sys};

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
    /// No page frames left for its memory.
    OutOfMemory,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SpawnError::TooManyPrograms => write!(f, "too many programs running (at most {})", Asid::COUNT - 1),
            SpawnError::ArgsTooLong => write!(f, "arguments longer than {} bytes", MAX_ARGS),
            SpawnError::OutOfMemory => write!(f, "out of memory"),
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
    /// Where its heap ends: the next `Map` starts here.
    heap_end: u64,
    /// Input sent to it and not read yet.
    input: VecDeque<u8>,
}

/// Most bytes of unread input a program can have waiting.
const MAX_INPUT: usize = 4096;

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
    let mut memory =
        AddressSpace::new(mmu::kernel_table(), &sys::memory::PAGE_FRAMES).map_err(|_| SpawnError::OutOfMemory)?;

    // Nothing else is mapped yet, and ELF segments are checked to fit below the stack, so
    // mapping can only fail for want of memory.
    let (entry, code_ranges, program_end) = match code {
        Code::Builtin(program) => {
            let code = programs::image();
            memory.map_range(CODE_BASE, code.len() as u64, Access::ReadExecute).map_err(out_of_memory)?;
            memory.load(CODE_BASE, code).expect("code pages are mapped");
            let end = CODE_BASE + code.len() as u64;
            (CODE_BASE + program.offset() as u64, alloc::vec![(CODE_BASE, code.len() as u64)], end)
        }
        Code::Elf(program) => {
            program.load(&mut memory).map_err(|error| match error {
                elf::LoadError::Map(error) => out_of_memory(error),
                elf::LoadError::Access(fault) => unreachable!("loading a mapped segment faulted: {:?}", fault),
            })?;
            let code = program.segments.iter().filter(|segment| segment.access == Access::ReadExecute);
            let end = program.segments.iter().map(elf::Segment::end).max().expect("programs have segments");
            (program.entry, code.map(|segment| (segment.address, segment.size)).collect(), end)
        }
    };
    for (start, len) in code_ranges {
        for va in (start..start + len).step_by(PAGE_SIZE) {
            let (frame, _) = memory.translate(va).expect("code pages are mapped");
            mmu::sync_instruction_cache(frame as usize, PAGE_SIZE);
        }
    }

    // The arguments go at the top of the stack, which starts just below them.
    memory.map_range(STACK_TOP - STACK_SIZE, STACK_SIZE, Access::ReadWrite).map_err(out_of_memory)?;
    let args_at = (STACK_TOP - args.len() as u64) & !15;
    memory.load(args_at, args.as_bytes()).expect("stack pages are mapped");

    let start = UserStart {
        translation_base: memory.translation_base(asid.0),
        entry,
        stack_top: args_at,
        args: [args_at, args.len() as u64],
    };
    let exit = Arc::new(IrqLock::new(None));
    let running = Running {
        name: name.into(),
        memory,
        _asid: asid,
        exit: exit.clone(),
        killed: false,
        heap_end: program_end.next_multiple_of(PAGE_SIZE as u64),
        input: VecDeque::new(),
    };
    // Registered under the lock, so the program can't make a system call before it is known.
    let id = PROCESSES.lock(|processes| {
        let id = sched::spawn_user(name, start);
        processes.insert(id, running);
        id
    });
    Ok(Process { id, exit })
}

/// Whether a program is running as task `id`.
pub fn is_running(id: TaskId) -> bool {
    PROCESSES.lock(|processes| processes.contains_key(&id))
}

#[derive(Debug)]
pub enum InputError {
    NotAProgram,
    /// It has `MAX_INPUT` bytes waiting already.
    Full,
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            InputError::NotAProgram => write!(f, "not a running program"),
            InputError::Full => write!(f, "input not taken: the program isn't reading"),
        }
    }
}

/// Gives the program running as task `id` a line of input, for its `Read`s.
pub fn send_line(id: TaskId, line: &str) -> Result<(), InputError> {
    PROCESSES.lock(|processes| {
        let running = processes.get_mut(&id).ok_or(InputError::NotAProgram)?;
        if running.input.len() + line.len() + 1 > MAX_INPUT {
            return Err(InputError::Full);
        }
        running.input.extend(line.as_bytes());
        running.input.push_back(b'\n');
        Ok(())
    })?;
    sched::notify(sched::PROGRAM_INPUT);
    Ok(())
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

/// Mapping into a fresh address space: only running out of memory can go wrong.
fn out_of_memory(error: MapError) -> SpawnError {
    match error {
        MapError::OutOfMemory => SpawnError::OutOfMemory,
        error => unreachable!("mapping a new program: {:?}", error),
    }
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
        Syscall::Map { len } => map(len),
        Syscall::Read { ptr, len } => read(ptr, len),
        Syscall::Random { ptr, len } => random(ptr, len),
    }
}

/// Fills the running program's memory at `ptr` with random bytes.
fn random(ptr: u64, len: u64) -> Result<u64, Errno> {
    let mut buf = [0; MAX_RANDOM];
    let buf = &mut buf[..len.min(MAX_RANDOM as u64) as usize];
    sys::random::fill(buf);
    let id = sched::current();
    PROCESSES.lock(|processes| {
        let running = processes.get_mut(&id).expect("a user task has a process");
        running.memory.write_user(ptr, buf).map_err(|_| Errno::Fault)
    })?;
    Ok(buf.len() as u64)
}

/// Copies the running program's waiting input into its memory at `ptr`, waiting for some if
/// there is none. A killed program stops waiting (and exits once the call returns).
fn read(ptr: u64, len: u64) -> Result<u64, Errno> {
    let id = sched::current();
    let len = len.min(MAX_READ as u64) as usize;
    if len == 0 {
        return Ok(0);
    }
    sched::wait_until(sched::PROGRAM_INPUT, || {
        PROCESSES.lock(|processes| processes.get(&id).is_none_or(|running| running.killed || !running.input.is_empty()))
    });
    PROCESSES.lock(|processes| {
        let running = processes.get_mut(&id).expect("a user task has a process");
        let mut buf = [0; MAX_READ];
        let count = len.min(running.input.len());
        for (slot, &byte) in buf.iter_mut().zip(running.input.iter()) {
            *slot = byte;
        }
        // Only taken from the queue once it has landed.
        running.memory.write_user(ptr, &buf[..count]).map_err(|_| Errno::Fault)?;
        running.input.drain(..count);
        Ok(count as u64)
    })
}

/// Grows the running program's heap by `len` bytes, in whole pages, up to `PROGRAM_END`.
/// Pages mapped before running out of memory stay mapped, past the returned heap end.
fn map(len: u64) -> Result<u64, Errno> {
    const PAGE: u64 = PAGE_SIZE as u64;
    let id = sched::current();
    // Only the program itself moves its heap end, and it is busy in here.
    let start = PROCESSES.lock(|processes| processes.get(&id).expect("a user task has a process").heap_end);
    let end = len
        .div_ceil(PAGE)
        .checked_mul(PAGE)
        .and_then(|len| start.checked_add(len))
        .filter(|&end| end <= PROGRAM_END)
        .ok_or(Errno::NoMemory)?;
    for va in (start..end).step_by(PAGE_SIZE) {
        // A page per lock, so a big heap doesn't keep interrupts masked for long.
        PROCESSES.lock(|processes| {
            let running = processes.get_mut(&id).expect("a user task has a process");
            running.memory.map(va, Access::ReadWrite)?;
            running.heap_end = va + PAGE;
            Ok(())
        })
        .map_err(|_: MapError| Errno::NoMemory)?;
    }
    Ok(start)
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
