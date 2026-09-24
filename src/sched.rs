// sched.rs
//! Tasks: each has its own kernel stack. Kernel tasks run at EL1; user tasks run a program
//! at EL0 (see `process`) and are at EL1, on that stack, only while handling an exception.
//!
//! A task that isn't running is an `ExceptionContext` saved on its stack by the exception
//! entry code. Switching tasks is returning a different context from the exception handler:
//! the timer IRQ preempts, and `svc #0` switches voluntarily (`yield_now`, `sleep`,
//! `wait_until`). Which task each core runs next is `rustypi_core::sched::RunQueue`'s
//! decision.
//!
//! A core that switches away from a task is still on that task's stack until the exception
//! entry code has moved to the next one's and called `sched_finish_switch`. Until then the
//! task is `LEAVING` that core, and no other core may pick it (or free its stack).
//!
//! Preemption can be held off with `no_preempt` (the `TryLock`s do so while held); a switch
//! that falls due meanwhile happens when the outermost `no_preempt` ends.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use core::time::Duration;
use rustypi_core::sched::{Event, RunQueue, State, TaskId};
use crate::arch::exception::ExceptionContext;
use crate::arch::fp::{self, FpState};
use crate::arch::mmu;
use crate::board::CORES;
use crate::drivers::{interrupt, timer};
use crate::synchronization::{interface::Mutex, IrqLock};
use crate::arch;

pub use rustypi_core::sched::TaskInfo;

/// The `svc` immediate that means "switch tasks".
pub const SVC_YIELD: u16 = 0;

/// Bytes the UART received.
pub const UART_RX: Event = Event(1);
/// A program was sent input.
pub const PROGRAM_INPUT: Event = Event(2);
/// The link task queued a frame for the shell.
pub const SHELL_INBOX: Event = Event(3);
/// A pipe changed: bytes went in or out, or an end closed.
pub const PIPE: Event = Event(4);

const STACK_SIZE: usize = 32 * 1024;
/// Unused stack holds this byte, so the high-water mark can be measured.
const STACK_FILL: u8 = 0x5A;
/// Written at the bottom of every stack and checked on every switch away from the task.
const CANARY: [u8; 16] = *b"RustyPI  canary!";
/// EL1h (the task uses SP_EL1) with all interrupts unmasked.
const SPSR_EL1H: u64 = 0b0101;
/// EL0 (the program uses SP_EL0) with all interrupts unmasked.
const SPSR_EL0T: u64 = 0b0000;
/// The SPSR bits that hold the exception level and stack pointer choice.
const SPSR_MODE: u64 = 0b1111;

struct Task {
    /// `None` for tasks on a stack of their own from boot: task 0 (the one `linker.ld`
    /// reserves) and the idle tasks of cores 1 to 3.
    stack: Option<Box<[u8]>>,
    /// Where its registers are saved while it isn't running.
    context: *mut ExceptionContext,
    /// Its address space, as a TTBR0 value: the kernel's, or its program's.
    translation_base: u64,
    /// A user task's FP/SIMD registers, as of when it last left a core (see `arch::fp`).
    fp: Option<Box<FpState>>,
    /// The core whose FP/SIMD registers it last loaded its values into.
    fp_core: Option<usize>,
    /// Where to send it the next time it is switched away from at EL0, for a kill that came
    /// while it was running.
    redirect: Option<extern "C" fn() -> !>,
}

impl Task {
    fn new(stack: Option<Box<[u8]>>, context: *mut ExceptionContext, translation_base: u64) -> Self {
        Task { stack, context, translation_base, fp: None, fp_core: None, redirect: None }
    }
}

struct Scheduler {
    queue: RunQueue,
    /// Indexed like the run queue's task ids.
    tasks: Vec<Option<Task>>,
}

// The raw context pointers point into stacks the scheduler owns.
unsafe impl Send for Scheduler {}

static SCHEDULER: IrqLock<Option<Scheduler>> = IrqLock::new(None);
/// Per core, the task (id plus one) it switched away from and may still be on the stack of;
/// 0 once it is off it.
static LEAVING: [AtomicUsize; CORES] = [const { AtomicUsize::new(0) }; CORES];
/// Per core: `no_preempt` depth, and whether a switch fell due meanwhile.
static PREEMPT_DISABLED: [AtomicU32; CORES] = [const { AtomicU32::new(0) }; CORES];
static SWITCH_PENDING: [AtomicBool; CORES] = [const { AtomicBool::new(false) }; CORES];

/// Turns the code running on core 0 into task 0, `name`, and starts core 0's idle task. Call
/// before enabling interrupts.
pub fn init(name: &'static str) {
    SCHEDULER.lock(|scheduler| {
        *scheduler = Some(Scheduler {
            queue: RunQueue::new(name, CORES),
            tasks: vec![Some(Task::new(None, core::ptr::null_mut(), mmu::kernel_translation_base()))],
        });
    });
    spawn_kernel("idle0", Box::new(|| idle()), Some(0));
}

/// What an idle task does: wait for an interrupt, over and over.
pub fn idle() -> ! {
    loop {
        unsafe { asm!("wfi", options(nomem, nostack)) };
    }
}

/// Starts a task running `entry`. It is removed once `entry` returns.
pub fn spawn(name: &'static str, entry: impl FnOnce() + 'static) -> TaskId {
    spawn_kernel(name, Box::new(entry), None)
}

/// Starts a kernel task, or the idle task of core `idle_for`.
fn spawn_kernel(name: &'static str, entry: Box<dyn FnOnce()>, idle_for: Option<usize>) -> TaskId {
    // A thin pointer to the closure, to pass in a register.
    let entry = Box::into_raw(Box::new(entry));
    let mut gpr = [0; 30];
    gpr[0] = entry as u64;
    let start = ExceptionContext {
        gpr,
        lr: 0,
        elr: task_entry as usize as u64,
        spsr: SPSR_EL1H,
        esr: 0,
        sp_el0: 0,
        _reserved: 0,
    };
    spawn_task(name, idle_for, start, mmu::kernel_translation_base(), None)
}

/// What a user task starts with. Addresses are in its own address space.
pub struct UserStart {
    /// TTBR0 for its address space.
    pub translation_base: u64,
    pub entry: u64,
    pub stack_top: u64,
    /// Passed in x0 and x1.
    pub args: [u64; 2],
}

/// Starts a task that runs a program at EL0. It leaves only through `exit`, called on its
/// behalf by `process` after `leave_user_space`.
pub fn spawn_user(name: &str, start: UserStart) -> TaskId {
    let mut gpr = [0; 30];
    gpr[..2].copy_from_slice(&start.args);
    let context = ExceptionContext {
        gpr,
        lr: 0,
        elr: start.entry,
        spsr: SPSR_EL0T,
        esr: 0,
        sp_el0: start.stack_top,
        _reserved: 0,
    };
    spawn_task(name, None, context, start.translation_base, Some(Box::new(FpState::new())))
}

/// Adds a task whose first switch "returns" from an exception into `start`.
fn spawn_task(
    name: &str,
    idle_for: Option<usize>,
    start: ExceptionContext,
    translation_base: u64,
    fp: Option<Box<FpState>>,
) -> TaskId {
    let mut stack = vec![STACK_FILL; STACK_SIZE].into_boxed_slice();
    stack[..CANARY.len()].copy_from_slice(&CANARY);

    // Once the first eret pops this context, SP_EL1 is the top of the stack, which is where
    // a user task's exceptions from EL0 will then land.
    let top = (stack.as_mut_ptr() as usize + STACK_SIZE) & !15;
    let context = (top - size_of::<ExceptionContext>()) as *mut ExceptionContext;
    unsafe { context.write(start) };

    SCHEDULER.lock(|scheduler| {
        let scheduler = scheduler.as_mut().expect("sched::init first");
        let id = match idle_for {
            Some(core) => scheduler.queue.add_idle(name, core),
            None => scheduler.queue.add(name),
        };
        scheduler.tasks.resize_with(id.0 + 1, || None);
        scheduler.tasks[id.0] = Some(Task { fp, ..Task::new(Some(stack), context, translation_base) });
        id
    })
}

extern "C" fn task_entry(entry: *mut Box<dyn FnOnce()>) -> ! {
    let entry = unsafe { Box::from_raw(entry) };
    entry();
    exit()
}

/// Runs `f` with the scheduler and the core this runs on, once the scheduler has started.
/// The core can't change meanwhile: IRQs are masked.
fn with_scheduler<R>(f: impl FnOnce(&mut Scheduler, usize) -> R) -> Option<R> {
    SCHEDULER.lock(|scheduler| scheduler.as_mut().map(|scheduler| f(scheduler, arch::core_id())))
}

/// Runs `f` on the run queue and the core this runs on, once the scheduler has started.
fn with_queue<R>(f: impl FnOnce(&mut RunQueue, usize) -> R) -> Option<R> {
    with_scheduler(|scheduler, core| f(&mut scheduler.queue, core))
}

/// Moves the running task into the kernel's address space, so its program's can be freed.
pub fn leave_user_space() {
    with_scheduler(|scheduler, core| {
        let current = scheduler.queue.current(core);
        let task = scheduler.tasks[current.0].as_mut().expect("the running task exists");
        task.translation_base = mmu::kernel_translation_base();
        mmu::set_translation_base(task.translation_base);
    })
    .expect("sched::init first");
}

/// Makes task `id`, if it is at EL0 (not in the middle of an exception or system call), run
/// `entry` at EL1 on its kernel stack instead when it next resumes. Returns whether it did.
/// If it is running on a core just now, that happens the next time the core switches away
/// from it at EL0 (at the latest, on its next tick), and this returns false.
pub fn redirect_to_kernel(id: TaskId, entry: extern "C" fn() -> !) -> bool {
    with_scheduler(|scheduler, _| {
        let running = scheduler.queue.core_of(id).is_some();
        let Some(task) = scheduler.tasks.get_mut(id.0).and_then(Option::as_mut) else { return false };
        if running || task.context.is_null() {
            task.redirect = Some(entry);
            return false;
        }
        // Not running, so its registers are in the context it was switched away in. (If a
        // core is still leaving it, that core no longer reads them.)
        redirect(unsafe { &mut *task.context }, entry)
    })
    .unwrap_or(false)
}

/// Points a context that would resume at EL0 at `entry` instead. Returns whether it did.
fn redirect(context: &mut ExceptionContext, entry: extern "C" fn() -> !) -> bool {
    if context.spsr & SPSR_MODE != SPSR_EL0T {
        return false;
    }
    context.elr = entry as usize as u64;
    context.spsr = SPSR_EL1H;
    true
}

/// Makes every task waiting for `event` (in `wait_until`) check again.
pub fn notify(event: Event) {
    with_queue(|queue, _| queue.notify(event));
}

/// Cuts a sleep or wait of task `id` short, so it notices something has changed. If it isn't
/// asleep or waiting yet, its next sleep or wait is cut short instead.
pub fn interrupt(id: TaskId) {
    with_queue(|queue, _| queue.interrupt(id));
}

/// Ends the running task. Its stack is freed on a later switch.
pub fn exit() -> ! {
    with_queue(|queue, core| queue.finish_current(core));
    yield_now();
    unreachable!("a finished task was scheduled again");
}

/// Lets other tasks run; returns when this one is picked again.
pub fn yield_now() {
    unsafe { asm!("svc #{}", const SVC_YIELD, options(nostack)) };
}

/// Sleeps for at least `duration` (rounded up to the next timer tick), letting other tasks
/// run meanwhile.
pub fn sleep(duration: Duration) {
    let until = timer::now_us() + duration.as_micros() as u64;
    with_queue(|queue, core| queue.sleep_current(core, until));
    yield_now();
}

/// Blocks until `ready()` holds, sleeping between `event` notifications instead of spinning.
pub fn wait_until(event: Event, ready: impl Fn() -> bool) {
    loop {
        // Looked at before checking, so a notify from another core after the check keeps
        // this task from waiting for one that already came.
        let seen = with_queue(|queue, _| queue.generation(event));
        if ready() {
            return;
        }
        if let Some(seen) = seen {
            with_queue(|queue, core| queue.wait_current(core, event, seen));
        }
        yield_now();
    }
}

/// The running task. Before `init`, the boot code counts as task 0, which it becomes.
pub fn current() -> TaskId {
    with_queue(|queue, core| queue.current(core)).unwrap_or(TaskId(0))
}

/// Blocks the running task until `wake` is called with its id, or returns straight away if
/// that happened already.
pub fn block() {
    with_queue(|queue, core| queue.block_current(core));
    yield_now();
}

/// Makes a task blocked in `block` ready to run, or keeps it from blocking next time.
pub fn wake(id: TaskId) {
    with_queue(|queue, _| queue.wake(id));
}

/// Waits for task `id` to finish (or be gone already).
pub fn join(id: TaskId) {
    loop {
        let state = with_queue(|queue, _| queue.state(id)).flatten();
        if matches!(state, None | Some(State::Finished)) {
            return;
        }
        sleep(Duration::from_millis(10));
    }
}

/// Runs `f` without being switched away from (interrupts are still handled), so it stays on
/// this core. A switch that falls due meanwhile happens once the outermost `no_preempt`
/// returns.
pub fn no_preempt<R>(f: impl FnOnce() -> R) -> R {
    // IRQs masked, so the task can't move between finding its core and counting.
    let saved = arch::irq_save();
    let core = arch::core_id();
    PREEMPT_DISABLED[core].fetch_add(1, Ordering::Relaxed);
    arch::irq_restore(saved);
    let result = f();
    let depth = PREEMPT_DISABLED[core].fetch_sub(1, Ordering::Relaxed) - 1;
    // Only switch from task context: inside an exception handler, IRQs are masked.
    if depth == 0 && arch::irqs_enabled() && SWITCH_PENDING[core].swap(false, Ordering::Relaxed) {
        yield_now();
    }
    result
}

/// Snapshot of every task, plus how much of each spawned task's stack has been used.
pub fn tasks() -> Vec<(TaskInfo, Option<(usize, usize)>)> {
    with_scheduler(|scheduler, _| {
        scheduler
            .queue
            .tasks()
            .into_iter()
            .map(|info| {
                let stack = scheduler.tasks[info.id.0].as_ref().and_then(|t| t.stack.as_ref());
                (info, stack.map(|stack| (stack_used(stack), stack.len())))
            })
            .collect()
    })
    .unwrap_or_default()
}

fn stack_used(stack: &[u8]) -> usize {
    let untouched = stack[CANARY.len()..].iter().take_while(|&&b| b == STACK_FILL).count();
    stack.len() - CANARY.len() - untouched
}

/// Gives the running task, a program that just trapped on an FP instruction, the FP/SIMD
/// registers: loads its values into them. Whoever had them before saved theirs when they
/// were switched away from. Called with IRQs masked.
pub fn take_fp_registers() {
    with_scheduler(|scheduler, core| {
        let current = scheduler.queue.current(core);
        let task = scheduler.tasks[current.0].as_mut().expect("the running task exists");
        fp::load(task.fp.as_ref().expect("programs have FP state"));
        fp::set_owner(core, Some(current));
        task.fp_core = Some(core);
        fp::allow_el0(true);
    })
    .expect("sched::init first");
}

/// Called from the IRQ vector: services the interrupts, then maybe switches tasks.
pub fn on_irq(ctx: *mut ExceptionContext) -> *mut ExceptionContext {
    let serviced = interrupt::handle();
    with_scheduler(|scheduler, core| {
        if serviced.uart {
            scheduler.queue.notify(UART_RX);
        }
        if serviced.timer {
            scheduler.queue.tick(core, timer::now_us());
        }
        if PREEMPT_DISABLED[core].load(Ordering::Relaxed) > 0 {
            if serviced.timer {
                SWITCH_PENDING[core].store(true, Ordering::Relaxed);
            }
            return ctx;
        }
        scheduler.switch(core, ctx, serviced.timer)
    })
    .unwrap_or(ctx)
}

/// Called for `svc #0`: the current task gives up the CPU.
pub fn on_yield(ctx: *mut ExceptionContext) -> *mut ExceptionContext {
    with_scheduler(|scheduler, core| scheduler.switch(core, ctx, true)).unwrap_or(ctx)
}

/// Called by the exception entry code once it has moved onto the stack of the task it
/// switched to: the task it left is free for other cores.
#[no_mangle]
extern "C" fn sched_finish_switch() {
    LEAVING[arch::core_id()].store(0, Ordering::Release);
}

/// Whether a core other than `core` is still on task `id`'s stack.
fn leaving_elsewhere(core: usize, id: TaskId) -> bool {
    LEAVING.iter().enumerate().any(|(other, leaving)| other != core && leaving.load(Ordering::Acquire) == id.0 + 1)
}

impl Scheduler {
    fn switch(&mut self, core: usize, ctx: *mut ExceptionContext, preempt: bool) -> *mut ExceptionContext {
        let current = self.queue.current(core);
        let mut current_base = None;
        if let Some(task) = self.tasks[current.0].as_mut() {
            task.context = ctx;
            current_base = Some(task.translation_base);
            if let Some(stack) = &task.stack {
                if stack[..CANARY.len()] != CANARY {
                    panic!("task {} overflowed its stack", current.0);
                }
            }
            // A kill that came while it ran: if it was at EL0, it goes to `exit` now.
            if let Some(entry) = task.redirect {
                if redirect(unsafe { &mut *ctx }, entry) {
                    task.redirect = None;
                }
            }
        }

        let next = self.queue.pick_next(core, preempt, |id| leaving_elsewhere(core, id));
        // Free finished tasks' stacks, except the one being left, which this handler is
        // still running on, and any another core is leaving. They go on a later switch.
        for id in self.queue.finished() {
            if id == current || leaving_elsewhere(core, id) {
                continue;
            }
            self.queue.remove(id);
            self.tasks[id.0] = None;
        }

        if next != current {
            if let Some(task) = self.tasks[current.0].as_mut() {
                // Saved now, so the task can carry on elsewhere with its own values.
                if fp::owner(core) == Some(current) {
                    if let Some(state) = task.fp.as_mut() {
                        fp::save(state);
                    }
                }
            }
            LEAVING[core].store(current.0 + 1, Ordering::Release);
        }
        let next_task = self.tasks[next.0].as_ref().expect("scheduled task exists");
        // The registers still hold its values only if nobody used them here since, and it
        // didn't load them on another core meanwhile.
        fp::allow_el0(fp::owner(core) == Some(next) && next_task.fp_core == Some(core));
        if current_base != Some(next_task.translation_base) {
            mmu::set_translation_base(next_task.translation_base);
        }
        next_task.context
    }
}
