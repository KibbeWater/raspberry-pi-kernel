// tasks.rs
//! Tasks and the scheduler.

use alloc::format;
use alloc::vec::Vec;
use core::time::Duration;
use rustypi_core::sched::CPU_WINDOW;
use rustypi_core::session::{LineKind, Reply};
use super::{Command, Outcome, Shell};
use crate::sched;
use crate::synchronization::{interface::Mutex as _, IrqLock, Mutex};
use crate::sys;

pub const COMMANDS: &[Command] = &[
    Command { name: "tasks", args: "[test]", description: "list tasks, or test preemption and Mutex", run: tasks },
];

fn tasks<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    match args {
        "" => list(reply),
        "test" => {
            preemption_test(reply);
            mutex_test(reply);
        }
        _ => return Outcome::Usage,
    }
    Outcome::Done
}

/// Every task, with its CPU use over the last second and in total.
fn list(reply: &mut Reply) {
    reply.line(LineKind::Rsp, format_args!(
        "{:>3}  {:<10} {:<9} {:>4} {:>9}  {}",
        "id", "name", "state", "cpu", "time", "stack",
    ));
    for (info, stack) in sched::tasks() {
        let cpu_ms = info.ticks * sys::TICK.as_millis() as u64;
        let stack = match stack {
            Some((used, size)) => format!("{}.{}/{} KB", used / 1024, used % 1024 * 10 / 1024, size / 1024),
            None => "boot stack".into(),
        };
        reply.line(LineKind::Rsp, format_args!(
            "{:>3}  {:<10} {:<9} {:>3}% {:>6}.{}s  {}",
            info.id.0,
            info.name,
            info.state.name(),
            info.recent_ticks * 100 / CPU_WINDOW,
            cpu_ms / 1000,
            cpu_ms % 1000 / 100,
            stack,
        ));
    }
}

const WORKERS: usize = 3;
const WORK_TIME: Duration = Duration::from_millis(200);
/// A jump in the clock this long between two loop iterations means another task ran.
const PREEMPTED_GAP_US: u64 = 2_000;

#[derive(Clone, Copy, Default)]
struct WorkerResult {
    start_us: u64,
    end_us: u64,
    preemptions: u32,
}

static RESULTS: IrqLock<[Option<WorkerResult>; WORKERS]> = IrqLock::new([None; WORKERS]);

/// Spins `WORK_TIME` without ever yielding, counting how often the clock jumped because the
/// scheduler switched to someone else.
fn worker(index: usize) {
    let start = sys::uptime().as_micros() as u64;
    let mut last = start;
    let mut preemptions = 0;
    loop {
        let now = sys::uptime().as_micros() as u64;
        if now - last > PREEMPTED_GAP_US {
            preemptions += 1;
        }
        last = now;
        if now - start >= WORK_TIME.as_micros() as u64 {
            break;
        }
    }
    let result = WorkerResult { start_us: start, end_us: last, preemptions };
    RESULTS.lock(|results| results[index] = Some(result));
}

/// Runs busy workers side by side: with preemption they overlap in time and each sees the
/// others take turns; without it they'd run one after another.
fn preemption_test(reply: &mut Reply) {
    RESULTS.lock(|results| *results = [None; WORKERS]);
    let ids: Vec<_> = (0..WORKERS).map(|i| sched::spawn("worker", move || worker(i))).collect();
    for id in ids {
        sched::join(id);
    }
    let results = RESULTS.lock(|results| *results);
    let Some(results) = results.into_iter().collect::<Option<Vec<_>>>() else {
        reply.rsp("tasks test FAILED: a worker didn't report");
        return;
    };
    let latest_start = results.iter().map(|r| r.start_us).max().unwrap();
    let earliest_end = results.iter().map(|r| r.end_us).min().unwrap();
    let overlapped = latest_start < earliest_end;
    let all_preempted = results.iter().all(|r| r.preemptions > 0);
    reply.line(LineKind::Rsp, format_args!(
        "tasks test {}: {} workers, overlapped {}, preempted {}/{}/{} times",
        if overlapped && all_preempted { "passed" } else { "FAILED" },
        WORKERS,
        if overlapped { "yes" } else { "no" },
        results[0].preemptions,
        results[1].preemptions,
        results[2].preemptions,
    ));
}

const INCREMENTS: u64 = 50;

static COUNTER: Mutex<u64> = Mutex::new(0);

/// Workers increment a shared counter, yielding while they hold the lock, so the others find
/// it held and must block. With a working Mutex no increment is lost.
fn mutex_test(reply: &mut Reply) {
    COUNTER.lock(|count| *count = 0);
    let ids: Vec<_> = (0..WORKERS)
        .map(|_| {
            sched::spawn("counter", || {
                for _ in 0..INCREMENTS {
                    COUNTER.lock(|count| {
                        let seen = *count;
                        sched::yield_now();
                        *count = seen + 1;
                    });
                }
            })
        })
        .collect();
    for id in ids {
        sched::join(id);
    }
    let count = COUNTER.lock(|count| *count);
    let expected = WORKERS as u64 * INCREMENTS;
    reply.line(LineKind::Rsp, format_args!(
        "mutex test {}: {}/{} increments",
        if count == expected { "passed" } else { "FAILED" },
        count,
        expected,
    ));
}
