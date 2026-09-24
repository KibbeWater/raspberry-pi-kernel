// tasks.rs
//! Kernel tasks and the scheduler.

use alloc::vec::Vec;
use core::time::Duration;
use rustypi_core::session::{LineKind, Reply};
use super::{Command, Outcome, Shell};
use crate::sched;
use crate::synchronization::{interface::Mutex, IrqLock};
use crate::sys;

pub const COMMANDS: &[Command] = &[
    Command { name: "tasks", args: "[test]", description: "list tasks, or test preemption", run: tasks },
];

fn tasks<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    match args {
        "" => list(reply),
        "test" => preemption_test(reply),
        _ => return Outcome::Usage,
    }
    Outcome::Done
}

fn list(reply: &mut Reply) {
    let tasks = sched::tasks();
    let total: u64 = tasks.iter().map(|(info, _)| info.ticks).sum::<u64>().max(1);
    reply.line(LineKind::Rsp, format_args!("{:>3}  {:<8} {:<9} {:>5}  {}", "id", "name", "state", "cpu", "stack"));
    for (info, stack) in tasks {
        let percent = info.ticks * 100 / total;
        match stack {
            Some((used, size)) => reply.line(LineKind::Rsp, format_args!(
                "{:>3}  {:<8} {:<9} {:>4}%  {}.{}/{} KB",
                info.id.0,
                info.name,
                info.state.name(),
                percent,
                used / 1024,
                used % 1024 * 10 / 1024,
                size / 1024,
            )),
            None => reply.line(LineKind::Rsp, format_args!(
                "{:>3}  {:<8} {:<9} {:>4}%  boot stack",
                info.id.0,
                info.name,
                info.state.name(),
                percent,
            )),
        }
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
