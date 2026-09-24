// programs.rs
//! User programs: starting the built-in ones, and testing them.

use alloc::vec::Vec;
use rustypi_core::session::{LineKind, Reply};
use super::{Command, Outcome, Shell};
use crate::arch::exception;
use crate::process::{self, Exit, PROGRAMS};

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
    match process::spawn(program) {
        Ok(process) => reply.line(LineKind::Rsp, format_args!("started {} as task {}", program.name, process.id().0)),
        Err(error) => reply.line(LineKind::Rsp, format_args!("run: {}", error)),
    }
    Outcome::Done
}

/// Runs every built-in program side by side and checks each ends as expected.
fn test(reply: &mut Reply) {
    let started: Vec<_> = PROGRAMS.iter().map(|program| (program, process::spawn(program))).collect();
    let mut passed = 0;
    for (program, process) in started {
        match process {
            Ok(process) => {
                let exit = process.wait();
                let ok = program.expected.matches(exit);
                passed += ok as usize;
                let verdict = if ok { "ok  " } else { "FAIL" };
                // The full exit line, addresses and all, is already on the console; the reply
                // must fit a link line.
                match exit {
                    Exit::Code(_) => reply.line(LineKind::Rsp, format_args!("{:<11} {} {}", program.name, verdict, exit)),
                    Exit::Crashed(fault) => reply.line(LineKind::Rsp, format_args!(
                        "{:<11} {} crashed: {}",
                        program.name,
                        verdict,
                        exception::class_name(fault.esr),
                    )),
                }
            }
            Err(error) => reply.line(LineKind::Rsp, format_args!("{:<11} FAIL {}", program.name, error)),
        }
    }
    reply.line(LineKind::Rsp, format_args!(
        "programs test {}: {}/{}",
        if passed == PROGRAMS.len() { "passed" } else { "FAILED" },
        passed,
        PROGRAMS.len(),
    ));
}
