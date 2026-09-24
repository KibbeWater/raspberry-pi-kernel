// programs.rs
//! User programs: running them from the SD card or the built-in ones, and testing them.

use alloc::vec::Vec;
use rustypi_core::elf;
use rustypi_core::sched::TaskId;
use rustypi_core::session::{LineKind, Reply};
use super::{Command, Outcome, Shell};
use crate::process::{self, Code, PROGRAMS};
use crate::sys;

pub const COMMANDS: &[Command] = &[
    Command { name: "programs", args: "[test]", description: "list built-in programs, or test them all", run: programs },
    Command { name: "run", args: "<program> [args]", description: "start a program; a trailing & runs it in the background", run: run_program },
    Command { name: "kill", args: "<task>", description: "stop a running program", run: kill },
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

/// Starts an ELF program from the SD card (anything with a `/` is a path) or a built-in one,
/// with the rest of the line as its arguments. Its output and exit follow as console lines:
/// the reply can't wait for them. It runs in the foreground, getting typed lines as input,
/// unless the line ends with `&`.
fn run_program<'a>(shell: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    let (args, background) = match args.strip_suffix('&') {
        Some(args) => (args.trim_end(), true),
        None => (args, false),
    };
    let (program, args) = match args.split_once(char::is_whitespace) {
        Some((program, args)) => (program, args.trim()),
        None => (args, ""),
    };
    if program.is_empty() {
        return Outcome::Usage;
    }

    let started = if program.contains('/') {
        let file = match sys::fs::read_file(program) {
            Ok(file) => file,
            Err(error) => {
                reply.line(LineKind::Rsp, format_args!("run: {}: {}", program, error));
                return Outcome::Done;
            }
        };
        let elf = match elf::parse(&file) {
            Ok(elf) => elf,
            Err(error) => {
                reply.line(LineKind::Rsp, format_args!("run: {}: {}", program, error.description()));
                return Outcome::Done;
            }
        };
        let name = program.rsplit('/').next().unwrap_or(program);
        process::spawn(name, Code::Elf(&elf), args)
    } else {
        let Some(builtin) = PROGRAMS.iter().find(|builtin| builtin.name == program) else {
            reply.line(LineKind::Rsp, format_args!("no built-in program '{}', see programs", program));
            return Outcome::Done;
        };
        process::spawn(builtin.name, Code::Builtin(builtin), args)
    };
    match started {
        Ok(process) if background => {
            reply.line(LineKind::Rsp, format_args!("started {} as task {} in the background", program, process.id().0));
        }
        Ok(process) => {
            shell.foreground = Some(process.id());
            reply.line(LineKind::Rsp, format_args!(
                "started {} as task {}; typing goes to it, !<command> to the shell",
                program,
                process.id().0,
            ));
        }
        Err(error) => reply.line(LineKind::Rsp, format_args!("run: {}", error)),
    }
    Outcome::Done
}

fn kill<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    let Ok(id) = args.parse() else {
        return Outcome::Usage;
    };
    match process::kill(TaskId(id)) {
        // The exit line follows once it has stopped.
        Ok(()) => reply.line(LineKind::Rsp, format_args!("killing task {}", id)),
        Err(error) => reply.line(LineKind::Rsp, format_args!("kill: task {}: {}", id, error)),
    }
    Outcome::Done
}

/// The arguments of the instances of each program `test` runs at once. They all use the same
/// addresses, so a leak between address spaces would show.
const INSTANCES: [&str; 2] = ["1", "2"];

/// Runs every built-in program side by side, twice, and checks each ends as expected.
fn test(reply: &mut Reply) {
    let started: Vec<_> = PROGRAMS
        .iter()
        .map(|program| {
            let spawn = |args| process::spawn(program.name, Code::Builtin(program), args);
            (program, INSTANCES.map(spawn))
        })
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
        INSTANCES.len(),
    ));
}
