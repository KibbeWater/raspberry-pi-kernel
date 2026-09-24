// sched.rs
//! Scheduling policy: which task runs next. Round robin over the ready tasks, with tasks
//! that sleep until a time or wait for an event, and an idle task that runs only when
//! nothing else can. Stacks and context switching are the kernel's business; this only
//! tracks task states.

use alloc::sync::Arc;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TaskId(pub usize);

/// Something tasks can wait for, like "the UART received bytes".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
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
    /// Timer ticks during which this task was running.
    pub ticks: u64,
}

struct Entry {
    name: Arc<str>,
    state: State,
    ticks: u64,
}

pub struct RunQueue {
    /// Indexed by `TaskId`; `None` once reaped. Ids are never reused.
    tasks: Vec<Option<Entry>>,
    current: TaskId,
    idle: Option<TaskId>,
}

impl RunQueue {
    /// A queue whose only task is the one already running, which becomes task 0.
    pub fn new(name: impl Into<Arc<str>>) -> Self {
        RunQueue {
            tasks: alloc::vec![Some(Entry { name: name.into(), state: State::Running, ticks: 0 })],
            current: TaskId(0),
            idle: None,
        }
    }

    pub fn current(&self) -> TaskId {
        self.current
    }

    /// Adds a ready task.
    pub fn add(&mut self, name: impl Into<Arc<str>>) -> TaskId {
        self.tasks.push(Some(Entry { name: name.into(), state: State::Ready, ticks: 0 }));
        TaskId(self.tasks.len() - 1)
    }

    /// Adds the task that runs when nothing else can. It is never picked otherwise.
    pub fn add_idle(&mut self, name: impl Into<Arc<str>>) -> TaskId {
        let id = self.add(name);
        self.idle = Some(id);
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

    fn set_current(&mut self, state: State) {
        let current = self.current;
        if let Some(entry) = self.entry_mut(current) {
            entry.state = state;
        }
    }

    /// The current task stops being runnable until `until_us`. Takes effect at the next
    /// `pick_next`.
    pub fn sleep_current(&mut self, until_us: u64) {
        self.set_current(State::Sleeping { until_us });
    }

    /// The current task stops being runnable until `event` is notified.
    pub fn wait_current(&mut self, event: Event) {
        self.set_current(State::Waiting(event));
    }

    /// The current task stops being runnable until `wake` is called with its id.
    pub fn block_current(&mut self) {
        self.set_current(State::Blocked);
    }

    /// Makes a blocked task ready. Returns whether it was blocked.
    pub fn wake(&mut self, id: TaskId) -> bool {
        match self.entry_mut(id) {
            Some(entry) if entry.state == State::Blocked => {
                entry.state = State::Ready;
                true
            }
            _ => false,
        }
    }

    pub fn finish_current(&mut self) {
        self.set_current(State::Finished);
    }

    /// Makes every task waiting for `event` ready. Returns whether any was.
    pub fn notify(&mut self, event: Event) -> bool {
        let mut woke = false;
        for entry in self.tasks.iter_mut().flatten() {
            if entry.state == State::Waiting(event) {
                entry.state = State::Ready;
                woke = true;
            }
        }
        woke
    }

    /// Accounts a timer tick to the current task and wakes sleepers whose time has come.
    pub fn tick(&mut self, now_us: u64) {
        let current = self.current;
        if let Some(entry) = self.entry_mut(current) {
            entry.ticks += 1;
        }
        for entry in self.tasks.iter_mut().flatten() {
            if let State::Sleeping { until_us } = entry.state {
                if until_us <= now_us {
                    entry.state = State::Ready;
                }
            }
        }
    }

    fn is_runnable(&self, id: TaskId) -> bool {
        matches!(self.state(id), Some(State::Running | State::Ready))
    }

    /// Chooses the task to run and marks it running. With `preempt`, a running task goes to
    /// the back of the line; without, it keeps running if it still can (unless it is the
    /// idle task and something else became ready).
    pub fn pick_next(&mut self, preempt: bool) -> TaskId {
        let current = self.current;
        let current_runnable = self.is_runnable(current);
        let is_idle = |id| Some(id) == self.idle;

        let next = if current_runnable && !preempt && !is_idle(current) {
            current
        } else {
            // Round robin: the first ready task after the current one, wrapping around.
            let count = self.tasks.len();
            (1..=count)
                .map(|offset| TaskId((current.0 + offset) % count))
                .find(|&id| !is_idle(id) && self.state(id) == Some(State::Ready))
                .or((current_runnable && !is_idle(current)).then_some(current))
                .or(self.idle)
                .unwrap_or(current)
        };

        if next != current && current_runnable {
            self.set_current(State::Ready);
        }
        self.current = next;
        self.set_current(State::Running);
        next
    }

    /// Finished tasks other than the current one, whose stacks can be freed.
    pub fn finished(&self) -> Vec<TaskId> {
        (0..self.tasks.len())
            .map(TaskId)
            .filter(|&id| id != self.current && self.state(id) == Some(State::Finished))
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
                entry.as_ref().map(|e| TaskInfo { id: TaskId(i), name: e.name.clone(), state: e.state, ticks: e.ticks })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INPUT: Event = Event(1);

    /// main (0), idle (1), a (2), b (3).
    fn queue() -> RunQueue {
        let mut queue = RunQueue::new("main");
        queue.add_idle("idle");
        queue.add("a");
        queue.add("b");
        queue
    }

    fn order(queue: &mut RunQueue, picks: usize) -> Vec<usize> {
        (0..picks).map(|_| queue.pick_next(true).0).collect()
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
        assert_eq!(queue.pick_next(false), TaskId(0));
        assert_eq!(queue.pick_next(false), TaskId(0));
    }

    #[test]
    fn a_lone_task_keeps_running_and_idle_runs_when_nothing_can() {
        let mut queue = RunQueue::new("main");
        let idle = queue.add_idle("idle");
        assert_eq!(queue.pick_next(true), TaskId(0));
        queue.sleep_current(100);
        assert_eq!(queue.pick_next(false), idle);
        // Idle gives way as soon as something is ready, even without preemption.
        queue.tick(100);
        assert_eq!(queue.pick_next(false), TaskId(0));
    }

    #[test]
    fn sleepers_wake_when_their_time_comes() {
        let mut queue = queue();
        queue.sleep_current(1_000);
        assert_eq!(order(&mut queue, 3), [2, 3, 2]);
        queue.tick(999);
        assert_eq!(queue.state(TaskId(0)), Some(State::Sleeping { until_us: 1_000 }));
        queue.tick(1_000);
        assert_eq!(order(&mut queue, 3), [3, 0, 2]);
    }

    #[test]
    fn waiting_tasks_run_again_once_notified() {
        let mut queue = queue();
        queue.wait_current(INPUT);
        assert_eq!(order(&mut queue, 3), [2, 3, 2]);
        assert!(!queue.notify(Event(2)));
        assert!(queue.notify(INPUT));
        assert_eq!(order(&mut queue, 2), [3, 0]);
    }

    #[test]
    fn blocked_tasks_run_again_only_when_woken_by_id() {
        let mut queue = queue();
        queue.block_current();
        assert!(!queue.notify(INPUT));
        assert_eq!(order(&mut queue, 3), [2, 3, 2]);
        assert!(!queue.wake(TaskId(3))); // not blocked
        assert!(queue.wake(TaskId(0)));
        assert_eq!(order(&mut queue, 2), [3, 0]);
    }

    #[test]
    fn finished_tasks_are_never_picked_and_can_be_reaped() {
        let mut queue = queue();
        queue.pick_next(true); // a runs
        queue.finish_current();
        assert_eq!(queue.finished(), []); // still current: its stack is in use
        assert_eq!(queue.pick_next(true), TaskId(3));
        assert_eq!(queue.finished(), [TaskId(2)]);
        queue.remove(TaskId(2));
        assert_eq!(order(&mut queue, 3), [0, 3, 0]);
        assert_eq!(queue.state(TaskId(2)), None);
    }

    #[test]
    fn ticks_are_accounted_to_the_running_task() {
        let mut queue = queue();
        queue.tick(0);
        queue.tick(0);
        queue.pick_next(true);
        queue.tick(0);
        let ticks: Vec<u64> = queue.tasks().iter().map(|t| t.ticks).collect();
        assert_eq!(ticks, [2, 0, 1, 0]);
    }
}
