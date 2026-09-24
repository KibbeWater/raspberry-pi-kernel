// sched.rs
//! Kernel tasks: each has its own stack and runs at EL1 in the kernel's address space.
//!
//! A task that isn't running is an `ExceptionContext` saved on its stack by the exception
//! entry code. Switching tasks is returning a different context from the exception handler:
//! the timer IRQ preempts, and `svc #0` switches voluntarily (`yield_now`, `sleep`,
//! `wait_until`). Which task runs next is `rustypi_core::sched::RunQueue`'s decision.
//!
//! Preemption can be held off with `no_preempt` (the `TryLock`s do so while held); a switch
//! that falls due meanwhile happens when the outermost `no_preempt` ends.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::time::Duration;
use rustypi_core::sched::{Event, RunQueue, State, TaskId};
use crate::arch::exception::ExceptionContext;
use crate::drivers::{interrupt, timer};
use crate::synchronization::{interface::Mutex, IrqLock};
use crate::arch;

pub use rustypi_core::sched::TaskInfo;

/// The `svc` immediate that means "switch tasks".
pub const SVC_YIELD: u16 = 0;

/// Bytes the UART received.
pub const UART_RX: Event = Event(1);

const STACK_SIZE: usize = 32 * 1024;
/// Unused stack holds this byte, so the high-water mark can be measured.
const STACK_FILL: u8 = 0x5A;
/// Written at the bottom of every stack and checked on every switch away from the task.
const CANARY: [u8; 16] = *b"RustyPI  canary!";
/// EL1h (the task uses SP_EL1) with all interrupts unmasked.
const SPSR_EL1H: u64 = 0b0101;

struct Task {
    /// `None` for the boot task, which runs on the stack `linker.ld` reserves.
    stack: Option<Box<[u8]>>,
    /// Where its registers are saved while it isn't running.
    context: *mut ExceptionContext,
}

struct Scheduler {
    queue: RunQueue,
    /// Indexed like the run queue's task ids.
    tasks: Vec<Option<Task>>,
}

// The raw context pointers point into stacks the scheduler owns.
unsafe impl Send for Scheduler {}

static SCHEDULER: IrqLock<Option<Scheduler>> = IrqLock::new(None);
static PREEMPT_DISABLED: AtomicU32 = AtomicU32::new(0);
static SWITCH_PENDING: AtomicBool = AtomicBool::new(false);

/// Turns the running code into task 0, `name`, and starts the idle task. Call before
/// enabling interrupts.
pub fn init(name: &'static str) {
    SCHEDULER.lock(|scheduler| {
        *scheduler = Some(Scheduler {
            queue: RunQueue::new(name),
            tasks: vec![Some(Task { stack: None, context: core::ptr::null_mut() })],
        });
    });
    spawn_task("idle", Box::new(idle), true);
}

fn idle() {
    loop {
        unsafe { asm!("wfi", options(nomem, nostack)) };
    }
}

/// Starts a task running `entry`. It is removed once `entry` returns.
pub fn spawn(name: &'static str, entry: impl FnOnce() + 'static) -> TaskId {
    spawn_task(name, Box::new(entry), false)
}

fn spawn_task(name: &'static str, entry: Box<dyn FnOnce()>, idle: bool) -> TaskId {
    let mut stack = vec![STACK_FILL; STACK_SIZE].into_boxed_slice();
    stack[..CANARY.len()].copy_from_slice(&CANARY);

    // The first switch to the task "returns" from an exception into `task_entry`.
    let top = (stack.as_mut_ptr() as usize + STACK_SIZE) & !15;
    let context = (top - size_of::<ExceptionContext>()) as *mut ExceptionContext;
    // A thin pointer to the closure, to pass in a register.
    let entry = Box::into_raw(Box::new(entry));
    let mut gpr = [0; 30];
    gpr[0] = entry as u64;
    unsafe {
        context.write(ExceptionContext {
            gpr,
            lr: 0,
            elr: task_entry as usize as u64,
            spsr: SPSR_EL1H,
            esr: 0,
        });
    }

    SCHEDULER.lock(|scheduler| {
        let scheduler = scheduler.as_mut().expect("sched::init first");
        let id = if idle { scheduler.queue.add_idle(name) } else { scheduler.queue.add(name) };
        scheduler.tasks.resize_with(id.0 + 1, || None);
        scheduler.tasks[id.0] = Some(Task { stack: Some(stack), context });
        id
    })
}

extern "C" fn task_entry(entry: *mut Box<dyn FnOnce()>) -> ! {
    let entry = unsafe { Box::from_raw(entry) };
    entry();
    SCHEDULER.lock(|scheduler| {
        if let Some(scheduler) = scheduler {
            scheduler.queue.finish_current();
        }
    });
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
    SCHEDULER.lock(|scheduler| {
        if let Some(scheduler) = scheduler {
            scheduler.queue.sleep_current(until);
        }
    });
    yield_now();
}

/// Blocks until `ready()` holds, sleeping between `event` notifications instead of spinning.
pub fn wait_until(event: Event, ready: impl Fn() -> bool) {
    loop {
        // With IRQs masked, the event can't fire between the check and going to sleep.
        let saved = arch::irq_save();
        if ready() {
            arch::irq_restore(saved);
            return;
        }
        SCHEDULER.lock(|scheduler| {
            if let Some(scheduler) = scheduler {
                scheduler.queue.wait_current(event);
            }
        });
        yield_now();
        arch::irq_restore(saved);
    }
}

/// The running task. Before `init`, the boot code counts as task 0, which it becomes.
pub fn current() -> TaskId {
    SCHEDULER.lock(|scheduler| scheduler.as_ref().map_or(TaskId(0), |s| s.queue.current()))
}

/// Blocks the running task until `wake` is called with its id. Call with IRQs masked, after
/// arranging for someone to wake it, so the wake can't come before the block.
pub fn block() {
    SCHEDULER.lock(|scheduler| {
        if let Some(scheduler) = scheduler {
            scheduler.queue.block_current();
        }
    });
    yield_now();
}

/// Makes a task blocked in `block` ready to run.
pub fn wake(id: TaskId) {
    SCHEDULER.lock(|scheduler| {
        if let Some(scheduler) = scheduler {
            scheduler.queue.wake(id);
        }
    });
}

/// Waits for task `id` to finish (or be gone already).
pub fn join(id: TaskId) {
    loop {
        let state = SCHEDULER.lock(|scheduler| scheduler.as_ref().and_then(|s| s.queue.state(id)));
        if matches!(state, None | Some(State::Finished)) {
            return;
        }
        sleep(Duration::from_millis(10));
    }
}

/// Runs `f` without being switched away from (interrupts are still handled). A switch that
/// falls due meanwhile happens once the outermost `no_preempt` returns.
pub fn no_preempt<R>(f: impl FnOnce() -> R) -> R {
    PREEMPT_DISABLED.store(PREEMPT_DISABLED.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
    let result = f();
    let depth = PREEMPT_DISABLED.load(Ordering::Relaxed) - 1;
    PREEMPT_DISABLED.store(depth, Ordering::Relaxed);
    // Only switch from task context: inside an exception handler, IRQs are masked.
    if depth == 0 && SWITCH_PENDING.load(Ordering::Relaxed) && arch::irqs_enabled() {
        SWITCH_PENDING.store(false, Ordering::Relaxed);
        yield_now();
    }
    result
}

/// Snapshot of every task, plus how much of each spawned task's stack has been used.
pub fn tasks() -> Vec<(TaskInfo, Option<(usize, usize)>)> {
    SCHEDULER.lock(|scheduler| {
        let Some(scheduler) = scheduler else { return Vec::new() };
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
}

fn stack_used(stack: &[u8]) -> usize {
    let untouched = stack[CANARY.len()..].iter().take_while(|&&b| b == STACK_FILL).count();
    stack.len() - CANARY.len() - untouched
}

/// Called from the IRQ vector: services the interrupts, then maybe switches tasks.
pub fn on_irq(ctx: *mut ExceptionContext) -> *mut ExceptionContext {
    let serviced = interrupt::handle();
    SCHEDULER.lock(|scheduler| {
        let Some(scheduler) = scheduler else { return ctx };
        if serviced.uart {
            scheduler.queue.notify(UART_RX);
        }
        if serviced.timer {
            scheduler.queue.tick(timer::now_us());
        }
        if PREEMPT_DISABLED.load(Ordering::Relaxed) > 0 {
            if serviced.timer {
                SWITCH_PENDING.store(true, Ordering::Relaxed);
            }
            return ctx;
        }
        scheduler.switch(ctx, serviced.timer)
    })
}

/// Called for `svc #0`: the current task gives up the CPU.
pub fn on_yield(ctx: *mut ExceptionContext) -> *mut ExceptionContext {
    SCHEDULER.lock(|scheduler| match scheduler {
        Some(scheduler) => scheduler.switch(ctx, true),
        None => ctx,
    })
}

impl Scheduler {
    fn switch(&mut self, ctx: *mut ExceptionContext, preempt: bool) -> *mut ExceptionContext {
        let current = self.queue.current();
        if let Some(task) = self.tasks[current.0].as_mut() {
            task.context = ctx;
            if let Some(stack) = &task.stack {
                if stack[..CANARY.len()] != CANARY {
                    panic!("task {} overflowed its stack", current.0);
                }
            }
        }

        let next = self.queue.pick_next(preempt);
        // Free finished tasks' stacks, except the one being left: this handler is still
        // running on it. It goes on a later switch.
        for id in self.queue.finished() {
            if id == current {
                continue;
            }
            self.queue.remove(id);
            self.tasks[id.0] = None;
        }
        self.tasks[next.0].as_ref().expect("scheduled task exists").context
    }
}
