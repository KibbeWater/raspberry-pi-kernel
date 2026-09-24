// sched.rs
//! Scheduling policy: which task each core runs next. Round robin over the ready tasks, with
//! tasks that sleep until a time, wait for an event or block until woken, and an idle task per
//! core that runs only when nothing else can. Stacks and context switching are the kernel's
//! business; this only tracks task states.
//!
//! Other cores keep running while a task decides to wait, so every way of waiting is safe
//! against its wake-up arriving first: `wait_current` checks the event's generation hasn't
//! moved since the task looked, and a `wake` or `interrupt` for a task that isn't (yet)
//! blocked or asleep is kept for its next `block_current`, `wait_current` or `sleep_current`.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TaskId(pub usize);

/// Something tasks can wait for, like "the UART received bytes".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event(pub u32);

/// How many times an event has been notified, so a task can tell whether it happened since
/// it last looked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Generation(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// On some core: see `TaskInfo::core`.
    Running,
    Ready,
    /// Until the given microsecond timestamp.
    Sleeping { until_us: u64 },
    Waiting(Event),
    /// Until another task wakes it by id, like a mutex handing it the lock.
    Blocked,
    /// Returned from its entry point; waiting to be reaped.
    Finished,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Ready => "ready",
            State::Sleeping { .. } => "sleeping",
            State::Waiting(_) => "waiting",
            State::Blocked => "blocked",
            State::Finished => "finished",
        }
    }
}

#[derive(Clone, Debug)]
pub struct TaskInfo {
    pub id: TaskId,
    pub name: Arc<str>,
    pub state: State,
    /// The core it is running on.
    pub core: Option<usize>,
    /// Timer ticks during which this task was running.
    pub ticks: u64,
    /// How many of the last `CPU_WINDOW` ticks it was running for, on whichever core.
    pub recent_ticks: u64,
}

/// Recent CPU use is measured over this many of core 0's ticks: a second at the kernel's
/// 10ms tick.
pub const CPU_WINDOW: u64 = 100;

struct Entry {
    name: Arc<str>,
    state: State,
    ticks: u64,
    /// Ticks in the current window, and in the last complete one.
    window_ticks: u64,
    recent_ticks: u64,
    /// A `wake` that came before the task blocked: its next `block_current` doesn't.
    wake_pending: bool,
    /// An `interrupt` that came while it wasn't asleep or waiting: its next sleep or wait
    /// doesn't happen.
    interrupt_pending: bool,
}

impl Entry {
    fn new(name: Arc<str>, state: State) -> Self {
        Entry { name, state, ticks: 0, window_ticks: 0, recent_ticks: 0, wake_pending: false, interrupt_pending: false }
    }
}

pub struct RunQueue {
    /// Indexed by `TaskId`; `None` once reaped. Ids are never reused.
    tasks: Vec<Option<Entry>>,
    /// What each core runs; `None` for a core that hasn't started.
    current: Vec<Option<TaskId>>,
    /// Each core's idle task.
    idle: Vec<Option<TaskId>>,
    /// Indexed by `Event`.
    generations: Vec<u64>,
    /// Core 0's ticks into the current CPU window.
    window_elapsed: u64,
}

impl RunQueue {
    /// A queue for `cores` cores, whose only task is the code already running on core 0,
    /// which becomes task 0.
    pub fn new(name: impl Into<Arc<str>>, cores: usize) -> Self {
        let mut current = vec![None; cores];
        current[0] = Some(TaskId(0));
        RunQueue {
            tasks: vec![Some(Entry::new(name.into(), State::Running))],
            current,
            idle: vec![None; cores],
            generations: Vec::new(),
            window_elapsed: 0,
        }
    }

    /// The task `core` is running. Panics for a core that hasn't started.
    pub fn current(&self, core: usize) -> TaskId {
        self.current[core].expect("the core has started")
    }

    /// The core task `id` is running on.
    pub fn core_of(&self, id: TaskId) -> Option<usize> {
        self.current.iter().position(|&current| current == Some(id))
    }

    /// Adds a ready task.
    pub fn add(&mut self, name: impl Into<Arc<str>>) -> TaskId {
        self.tasks.push(Some(Entry::new(name.into(), State::Ready)));
        TaskId(self.tasks.len() - 1)
    }

    /// Adds the task `core` runs when nothing else can. It runs nowhere else, and never when
    /// something else could.
    pub fn add_idle(&mut self, name: impl Into<Arc<str>>, core: usize) -> TaskId {
        let id = self.add(name);
        self.idle[core] = Some(id);
        id
    }

    /// Starts `core`: the code running on it becomes its idle task.
    pub fn start_core(&mut self, name: impl Into<Arc<str>>, core: usize) -> TaskId {
        assert!(self.current[core].is_none(), "core {core} started twice");
        let id = self.add_idle(name, core);
        self.set_state(id, State::Running);
        self.current[core] = Some(id);
        id
    }

    pub fn state(&self, id: TaskId) -> Option<State> {
        self.entry(id).map(|entry| entry.state)
    }

    fn entry(&self, id: TaskId) -> Option<&Entry> {
        self.tasks.get(id.0)?.as_ref()
    }

    fn entry_mut(&mut self, id: TaskId) -> Option<&mut Entry> {
        self.tasks.get_mut(id.0)?.as_mut()
    }

    fn set_state(&mut self, id: TaskId, state: State) {
        if let Some(entry) = self.entry_mut(id) {
            entry.state = state;
        }
    }

    fn is_idle(&self, id: TaskId) -> bool {
        self.idle.contains(&Some(id))
    }

    /// The current task of `core`, taking a pending interrupt if it has one.
    fn take_interrupt(&mut self, core: usize) -> (TaskId, bool) {
        let current = self.current(core);
        let pending = self.entry_mut(current).is_some_and(|entry| core::mem::take(&mut entry.interrupt_pending));
        (current, pending)
    }

    /// The current task of `core` stops being runnable until `until_us`, unless it was
    /// interrupted meanwhile. Takes effect at the next `pick_next`.
    pub fn sleep_current(&mut self, core: usize, until_us: u64) {
        let (current, interrupted) = self.take_interrupt(core);
        if !interrupted {
            self.set_state(current, State::Sleeping { until_us });
        }
    }

    /// How many times `event` has been notified.
    pub fn generation(&self, event: Event) -> Generation {
        Generation(self.generations.get(event.0 as usize).copied().unwrap_or(0))
    }

    /// The current task of `core` stops being runnable until `event` is notified. Unless it
    /// already was since the task saw generation `seen` (or the task was interrupted): then
    /// it stays runnable, to look again.
    pub fn wait_current(&mut self, core: usize, event: Event, seen: Generation) {
        let (current, interrupted) = self.take_interrupt(core);
        if !interrupted && self.generation(event) == seen {
            self.set_state(current, State::Waiting(event));
        }
    }

    /// The current task of `core` stops being runnable until `wake` is called with its id,
    /// unless that already happened.
    pub fn block_current(&mut self, core: usize) {
        let current = self.current(core);
        let woken = self.entry_mut(current).is_some_and(|entry| core::mem::take(&mut entry.wake_pending));
        if !woken {
            self.set_state(current, State::Blocked);
        }
    }

    /// Makes a blocked task ready. Returns whether it was blocked; if it wasn't, its next
    /// `block_current` returns straight away.
    pub fn wake(&mut self, id: TaskId) -> bool {
        match self.entry_mut(id) {
            Some(entry) if entry.state == State::Blocked => {
                entry.state = State::Ready;
                true
            }
            Some(entry) if entry.state != State::Finished => {
                entry.wake_pending = true;
                false
            }
            _ => false,
        }
    }

    /// Makes a sleeping or waiting task ready early, so it notices something has changed
    /// (like being killed). Returns whether it was sleeping or waiting; if it was running or
    /// ready, its next sleep or wait doesn't happen. A blocked task waits on for its lock.
    pub fn interrupt(&mut self, id: TaskId) -> bool {
        match self.entry_mut(id) {
            Some(entry) if matches!(entry.state, State::Sleeping { .. } | State::Waiting(_)) => {
                entry.state = State::Ready;
                true
            }
            Some(entry) if matches!(entry.state, State::Running | State::Ready) => {
                entry.interrupt_pending = true;
                false
            }
            _ => false,
        }
    }

    pub fn finish_current(&mut self, core: usize) {
        let current = self.current(core);
        self.set_state(current, State::Finished);
    }

    /// Makes every task waiting for `event` ready, and moves its generation on. Returns
    /// whether any task was waiting.
    pub fn notify(&mut self, event: Event) -> bool {
        let index = event.0 as usize;
        if self.generations.len() <= index {
            self.generations.resize(index + 1, 0);
        }
        self.generations[index] += 1;
        let mut woke = false;
        for entry in self.tasks.iter_mut().flatten() {
            if entry.state == State::Waiting(event) {
                entry.state = State::Ready;
                woke = true;
            }
        }
        woke
    }

    /// Accounts a timer tick on `core` to its current task, and wakes sleepers whose time has
    /// come. Core 0's ticks also measure out the CPU window.
    pub fn tick(&mut self, core: usize, now_us: u64) {
        let current = self.current(core);
        if let Some(entry) = self.entry_mut(current) {
            entry.ticks += 1;
            entry.window_ticks += 1;
        }
        if core == 0 {
            self.window_elapsed += 1;
            if self.window_elapsed == CPU_WINDOW {
                self.window_elapsed = 0;
                for entry in self.tasks.iter_mut().flatten() {
                    entry.recent_ticks = core::mem::take(&mut entry.window_ticks);
                }
            }
        }
        for entry in self.tasks.iter_mut().flatten() {
            if let State::Sleeping { until_us } = entry.state {
                if until_us <= now_us {
                    entry.state = State::Ready;
                }
            }
        }
    }

    /// Chooses the task `core` runs next and marks it running. With `preempt`, its running
    /// task goes to the back of the line; without, it keeps running if it still can (unless
    /// it is the idle task and something else became ready). Tasks `busy` says another core
    /// is still switching away from aren't picked.
    pub fn pick_next(&mut self, core: usize, preempt: bool, busy: impl Fn(TaskId) -> bool) -> TaskId {
        let current = self.current(core);
        let current_runnable = self.state(current) == Some(State::Running);
        let keep_current = current_runnable && !self.is_idle(current);

        let next = if keep_current && !preempt {
            current
        } else {
            // Round robin: the first ready task after the current one, wrapping around.
            let count = self.tasks.len();
            (1..=count)
                .map(|offset| TaskId((current.0 + offset) % count))
                .find(|&id| self.state(id) == Some(State::Ready) && !self.is_idle(id) && !busy(id))
                .or(keep_current.then_some(current))
                .or(self.idle[core])
                .unwrap_or(current)
        };

        if next != current && current_runnable {
            self.set_state(current, State::Ready);
        }
        self.current[core] = Some(next);
        self.set_state(next, State::Running);
        next
    }

    /// Finished tasks no core is running, whose stacks can be freed once no core is still
    /// switching away from them.
    pub fn finished(&self) -> Vec<TaskId> {
        (0..self.tasks.len())
            .map(TaskId)
            .filter(|&id| self.core_of(id).is_none() && self.state(id) == Some(State::Finished))
            .collect()
    }

    /// Forgets a task. Its id is not reused.
    pub fn remove(&mut self, id: TaskId) {
        if let Some(slot) = self.tasks.get_mut(id.0) {
            *slot = None;
        }
    }

    pub fn tasks(&self) -> Vec<TaskInfo> {
        self.tasks
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                entry.as_ref().map(|e| TaskInfo {
                    id: TaskId(i),
                    name: e.name.clone(),
                    state: e.state,
                    core: self.core_of(TaskId(i)),
                    ticks: e.ticks,
                    recent_ticks: e.recent_ticks,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INPUT: Event = Event(1);

    fn none(_: TaskId) -> bool {
        false
    }

    /// One core: main (0), idle (1), a (2), b (3).
    fn queue() -> RunQueue {
        let mut queue = RunQueue::new("main", 1);
        queue.add_idle("idle", 0);
        queue.add("a");
        queue.add("b");
        queue
    }

    fn order(queue: &mut RunQueue, picks: usize) -> Vec<usize> {
        (0..picks).map(|_| queue.pick_next(0, true, none).0).collect()
    }

    #[test]
    fn preemption_rotates_through_ready_tasks_and_skips_idle() {
        let mut queue = queue();
        assert_eq!(order(&mut queue, 6), [2, 3, 0, 2, 3, 0]);
        assert_eq!(queue.state(TaskId(0)), Some(State::Running));
        assert_eq!(queue.state(TaskId(2)), Some(State::Ready));
    }

    #[test]
    fn without_preemption_the_current_task_keeps_running() {
        let mut queue = queue();
        assert_eq!(queue.pick_next(0, false, none), TaskId(0));
        assert_eq!(queue.pick_next(0, false, none), TaskId(0));
    }

    #[test]
    fn a_lone_task_keeps_running_and_idle_runs_when_nothing_can() {
        let mut queue = RunQueue::new("main", 1);
        let idle = queue.add_idle("idle", 0);
        assert_eq!(queue.pick_next(0, true, none), TaskId(0));
        queue.sleep_current(0, 100);
        assert_eq!(queue.pick_next(0, false, none), idle);
        // Idle gives way as soon as something is ready, even without preemption.
        queue.tick(0, 100);
        assert_eq!(queue.pick_next(0, false, none), TaskId(0));
    }

    #[test]
    fn sleepers_wake_when_their_time_comes() {
        let mut queue = queue();
        queue.sleep_current(0, 1_000);
        assert_eq!(order(&mut queue, 3), [2, 3, 2]);
        queue.tick(0, 999);
        assert_eq!(queue.state(TaskId(0)), Some(State::Sleeping { until_us: 1_000 }));
        queue.tick(0, 1_000);
        assert_eq!(order(&mut queue, 3), [3, 0, 2]);
    }

    #[test]
    fn waiting_tasks_run_again_once_notified() {
        let mut queue = queue();
        let seen = queue.generation(INPUT);
        queue.wait_current(0, INPUT, seen);
        assert_eq!(order(&mut queue, 3), [2, 3, 2]);
        assert!(!queue.notify(Event(2)));
        assert!(queue.notify(INPUT));
        assert_eq!(order(&mut queue, 2), [3, 0]);
    }

    #[test]
    fn a_notify_after_looking_keeps_the_task_from_waiting() {
        let mut queue = queue();
        let seen = queue.generation(INPUT);
        // Another core notifies between the task checking its condition and waiting.
        queue.notify(INPUT);
        queue.wait_current(0, INPUT, seen);
        assert_eq!(queue.state(TaskId(0)), Some(State::Running));
        // With nothing new, it waits.
        queue.wait_current(0, INPUT, queue.generation(INPUT));
        assert_eq!(queue.state(TaskId(0)), Some(State::Waiting(INPUT)));
    }

    #[test]
    fn blocked_tasks_run_again_only_when_woken_by_id() {
        let mut queue = queue();
        queue.block_current(0);
        assert!(!queue.notify(INPUT));
        assert_eq!(order(&mut queue, 3), [2, 3, 2]);
        assert!(!queue.wake(TaskId(3))); // not blocked
        assert!(queue.wake(TaskId(0)));
        assert_eq!(order(&mut queue, 2), [3, 0]);
    }

    #[test]
    fn a_wake_before_blocking_keeps_the_task_from_blocking() {
        let mut queue = queue();
        assert!(!queue.wake(TaskId(0))); // it's running: the wake is kept
        queue.block_current(0);
        assert_eq!(queue.state(TaskId(0)), Some(State::Running));
        queue.block_current(0); // used up
        assert_eq!(queue.state(TaskId(0)), Some(State::Blocked));
    }

    #[test]
    fn finished_tasks_are_never_picked_and_can_be_reaped() {
        let mut queue = queue();
        queue.pick_next(0, true, none); // a runs
        queue.finish_current(0);
        assert_eq!(queue.finished(), []); // still current: its stack is in use
        assert_eq!(queue.pick_next(0, true, none), TaskId(3));
        assert_eq!(queue.finished(), [TaskId(2)]);
        queue.remove(TaskId(2));
        assert_eq!(order(&mut queue, 3), [0, 3, 0]);
        assert_eq!(queue.state(TaskId(2)), None);
        assert!(!queue.wake(TaskId(2)));
    }

    #[test]
    fn recent_cpu_use_covers_the_last_complete_window() {
        let mut queue = queue();
        let recent = |queue: &RunQueue| queue.tasks().iter().map(|t| t.recent_ticks).collect::<Vec<_>>();
        // main runs the first quarter of a window, then a the rest.
        for _ in 0..CPU_WINDOW / 4 {
            queue.tick(0, 0);
        }
        queue.pick_next(0, true, none);
        for _ in 0..CPU_WINDOW / 4 * 3 - 1 {
            queue.tick(0, 0);
        }
        assert_eq!(recent(&queue), [0, 0, 0, 0]); // the window isn't over yet
        queue.tick(0, 0);
        assert_eq!(recent(&queue), [CPU_WINDOW / 4, 0, CPU_WINDOW / 4 * 3, 0]);
        // A whole window of a keeps the figures until it completes, then replaces them.
        for _ in 0..CPU_WINDOW {
            queue.tick(0, 0);
        }
        assert_eq!(recent(&queue), [0, 0, CPU_WINDOW, 0]);
    }

    #[test]
    fn interrupting_wakes_sleepers_and_waiters_and_is_kept_for_the_running() {
        let mut queue = queue();
        queue.sleep_current(0, 1_000);
        assert_eq!(queue.pick_next(0, false, none), TaskId(2));
        assert!(queue.interrupt(TaskId(0)));
        assert_eq!(queue.state(TaskId(0)), Some(State::Ready));
        queue.wait_current(0, INPUT, queue.generation(INPUT));
        assert_eq!(queue.pick_next(0, false, none), TaskId(3));
        assert!(queue.interrupt(TaskId(2)));
        queue.block_current(0);
        assert!(!queue.interrupt(TaskId(3))); // a blocked task waits for its lock
        assert!(!queue.interrupt(TaskId(99)));
        // A ready task's next sleep is cut short before it starts.
        assert!(!queue.interrupt(TaskId(0)));
        assert_eq!(queue.pick_next(0, false, none), TaskId(0));
        queue.sleep_current(0, 1_000);
        assert_eq!(queue.state(TaskId(0)), Some(State::Running));
        queue.sleep_current(0, 1_000);
        assert_eq!(queue.state(TaskId(0)), Some(State::Sleeping { until_us: 1_000 }));
    }

    #[test]
    fn ticks_are_accounted_to_the_running_task() {
        let mut queue = queue();
        queue.tick(0, 0);
        queue.tick(0, 0);
        queue.pick_next(0, true, none);
        queue.tick(0, 0);
        let ticks: Vec<u64> = queue.tasks().iter().map(|t| t.ticks).collect();
        assert_eq!(ticks, [2, 0, 1, 0]);
    }

    /// Two cores: main (0) on core 0, idle0 (1), idle1 (2) running on core 1, a (3), b (4).
    fn two_cores() -> RunQueue {
        let mut queue = RunQueue::new("main", 2);
        queue.add_idle("idle0", 0);
        queue.start_core("idle1", 1);
        queue.add("a");
        queue.add("b");
        queue
    }

    #[test]
    fn cores_never_run_the_same_task() {
        let mut queue = two_cores();
        assert_eq!(queue.core_of(TaskId(2)), Some(1));
        for _ in 0..10 {
            let first = queue.pick_next(0, true, none);
            let second = queue.pick_next(1, true, none);
            assert_ne!(first, second);
            assert_eq!(queue.core_of(first), Some(0));
            assert_eq!(queue.core_of(second), Some(1));
        }
    }

    #[test]
    fn each_core_idles_on_its_own_idle_task() {
        let mut queue = two_cores();
        // main and a run; b sleeps, so core 1 has nothing when a sleeps too.
        assert_eq!(queue.pick_next(1, false, none), TaskId(3));
        queue.pick_next(0, true, none); // main runs b next, then comes back
        queue.sleep_current(0, 10);
        assert_eq!(queue.pick_next(0, false, none), TaskId(0));
        queue.sleep_current(1, 10);
        assert_eq!(queue.pick_next(1, false, none), TaskId(2));
        queue.sleep_current(0, 10);
        assert_eq!(queue.pick_next(0, false, none), TaskId(1));
    }

    #[test]
    fn a_task_another_core_is_leaving_waits() {
        let mut queue = two_cores();
        // Core 0 switches from main to a; until it is off main's stack, core 1 leaves it.
        assert_eq!(queue.pick_next(0, true, none), TaskId(3));
        assert_eq!(queue.pick_next(1, false, |id| id == TaskId(0)), TaskId(4));
        queue.sleep_current(1, 10);
        assert_eq!(queue.pick_next(1, false, |id| id == TaskId(0)), TaskId(2));
        assert_eq!(queue.pick_next(1, false, none), TaskId(0));
    }

    #[test]
    fn only_core_0_measures_the_cpu_window_but_every_core_counts_ticks() {
        let mut queue = two_cores();
        queue.pick_next(1, false, none); // core 1 runs a
        for _ in 0..CPU_WINDOW {
            queue.tick(1, 0);
        }
        assert!(queue.tasks().iter().all(|t| t.recent_ticks == 0));
        for _ in 0..CPU_WINDOW {
            queue.tick(0, 0);
        }
        let recent: Vec<u64> = queue.tasks().iter().map(|t| t.recent_ticks).collect();
        assert_eq!(recent, [CPU_WINDOW, 0, 0, CPU_WINDOW, 0]);
    }

    #[test]
    fn a_finished_task_is_reaped_only_once_no_core_runs_it() {
        let mut queue = two_cores();
        queue.pick_next(1, false, none); // core 1 runs a
        queue.finish_current(1);
        assert_eq!(queue.finished(), []);
        queue.pick_next(1, false, none);
        assert_eq!(queue.finished(), [TaskId(3)]);
    }
}
