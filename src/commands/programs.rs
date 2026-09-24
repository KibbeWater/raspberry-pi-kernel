// programs.rs
//! User programs: starting the built-in ones, and testing them.

use alloc::vec::Vec;
use rustypi_core::session::{LineKind, Reply};
use super::{Command, Outcome, Shell};
use crate::process::{self, PROGRAMS};

pub const COMMANDS: &[Command] = &[
    Command { name: "programs", args: "[test]", description: "list built-in programs, or test them all", run: programs },
    Command { name: "run", args: "<program>", description: "start a built-in program at EL0", run: run_program },
];

fn programs<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    match args {
        "" => {
            for program in PROGRAMS {
                reply.line(LineKind::Rsp, format_args!("{:<11} {}", program.name, program.description));
            }
        }
        "test" => test(reply),
        _ => return Outcome::Usage,
    }
    Outcome::Done
}

fn run_program<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    if args.is_empty() {
        return Outcome::Usage;
    }
    let Some(program) = PROGRAMS.iter().find(|program| program.name == args) else {
        reply.line(LineKind::Rsp, format_args!("no program '{}', see programs", args));
        return Outcome::Done;
    };
    // Its output and exit follow as console lines; the reply can't wait for them.
    match process::spawn(program, 1) {
        Ok(process) => reply.line(LineKind::Rsp, format_args!("started {} as task {}", program.name, process.id().0)),
        Err(error) => reply.line(LineKind::Rsp, format_args!("run: {}", error)),
    }
    Outcome::Done
}

/// How many of each program `test` runs at once. They all use the same addresses, so a leak
/// between address spaces would show.
const INSTANCES: u64 = 2;

/// Runs every built-in program side by side, twice, and checks each ends as expected. Each
/// instance gets its number (from 1) in x0.
fn test(reply: &mut Reply) {
    let started: Vec<_> = PROGRAMS
        .iter()
        .map(|program| (program, (1..=INSTANCES).map(|arg| process::spawn(program, arg)).collect::<Vec<_>>()))
        .collect();
    let mut passed = 0;
    for (program, processes) in started {
        let results: Vec<_> = processes.into_iter().map(|process| process.map(|process| process.wait())).collect();
        let failure = results.iter().find(|result| !matches!(result, Ok(exit) if program.expected.matches(*exit)));
        passed += failure.is_none() as usize;
        let verdict = if failure.is_none() { "ok  " } else { "FAIL" };
        // A failing instance if there is one, else the first.
        match failure.unwrap_or(&results[0]) {
            Ok(exit) => reply.line(LineKind::Rsp, format_args!("{:<11} {} {}", program.name, verdict, exit)),
            Err(error) => reply.line(LineKind::Rsp, format_args!("{:<11} {} {}", program.name, verdict, error)),
        }
    }
    reply.line(LineKind::Rsp, format_args!(
        "programs test {}: {}/{}, {} of each at once",
        if passed == PROGRAMS.len() { "passed" } else { "FAILED" },
        passed,
        PROGRAMS.len(),
        INSTANCES,
    ));
}
