// commands/mod.rs
//! Commands typed on the host, delivered by the Arduino bridge as `MSG` frames.
//!
//! Every command is a `Command` in one of the `GROUPS` tables. `help` and the dispatcher
//! both read those tables, so the help text can't drift from what is actually accepted.
//! Handlers write their answer into a `session::Reply`.
//!
//! A line that isn't a command but names a program runs it, like `run`: a path, or a name in
//! `/bin`, and a trailing `&` runs it in the background. While a program runs in the
//! foreground, lines go to it as input instead; a line starting with `!` is a command either
//! way.

mod led;
mod memory;
mod net;
mod programs;
mod screen;
mod storage;
mod system;
mod tasks;
mod usb;

use alloc::format;
use alloc::string::String;
use rustypi_core::sched::TaskId;
use rustypi_core::session::{LineKind, Reply};
use crate::process;

pub use system::Action;

/// A command handler: gets everything after the command name, trimmed.
type Handler = for<'a> fn(&mut Shell, &'a str, &mut Reply) -> Outcome<'a>;

pub struct Command {
    /// The first word, which selects the command.
    pub name: &'static str,
    /// What may follow the name, for `help` and usage errors: `on|off|toggle`.
    pub args: &'static str,
    pub description: &'static str,
    pub run: Handler,
}

pub enum Outcome<'a> {
    Done,
    /// The arguments didn't fit; the dispatcher replies with the usage line.
    Usage,
    /// Done, and this action follows once the reply has been sent.
    Then(Action<'a>),
}

const HELP: &[Command] = &[Command {
    name: "help",
    args: "",
    description: "list commands",
    run: |_, args, reply| {
        if !args.is_empty() {
            return Outcome::Usage;
        }
        for command in commands() {
            let usage = usage(command);
            reply.line(LineKind::Help, format_args!("{:<20} {}", usage, command.description));
        }
        reply.line(LineKind::Help, format_args!("{:<20} {}", "<program> [args] [&]", "run a program from /bin, like run"));
        Outcome::Done
    },
}];

const GROUPS: &[&[Command]] = &[
    HELP,
    system::COMMANDS,
    led::COMMANDS,
    memory::COMMANDS,
    screen::COMMANDS,
    storage::COMMANDS,
    tasks::COMMANDS,
    programs::COMMANDS,
    usb::COMMANDS,
    net::COMMANDS,
];

fn commands() -> impl Iterator<Item = &'static Command> {
    GROUPS.iter().flat_map(|group| group.iter())
}

fn usage(command: &Command) -> String {
    if command.args.is_empty() {
        command.name.into()
    } else {
        format!("{} {}", command.name, command.args)
    }
}

/// State that commands keep between calls.
pub struct Shell {
    led: led::Led,
    /// The program that gets typed lines as input, if it is still running.
    foreground: Option<TaskId>,
    /// The current directory: absolute and resolved. Programs started from here begin in it.
    cwd: String,
}

impl Shell {
    pub fn new() -> Self {
        Shell { led: led::Led::new(), foreground: None, cwd: "/".into() }
    }

    /// `path` from the current directory.
    fn path(&self, path: &str) -> String {
        rustypi_core::path::resolve(&self.cwd, path)
    }

    /// Ctrl+C on the local keyboard: stops the foreground program, if there is one. (It says
    /// it was killed as it exits.)
    pub fn interrupt(&mut self) {
        if let Some(id) = self.foreground.take() {
            let _ = process::kill(id);
        }
    }

    /// Runs one command line, or passes it to the foreground program. Returns an action to
    /// perform after the reply is sent.
    pub fn handle<'a>(&mut self, text: &'a str, reply: &mut Reply) -> Option<Action<'a>> {
        let text = match text.strip_prefix('!') {
            Some(command) => command,
            None => {
                if let Some(id) = self.foreground {
                    if process::is_running(id) {
                        match process::send_line(id, text) {
                            // Said once per line that goes to a program that isn't asking,
                            // so typing doesn't seem to vanish.
                            Ok(delivered) if !delivered.reading => reply.line(LineKind::Rsp, format_args!(
                                "{} (task {}) isn't reading input; the line waits for it. !<command> for the shell",
                                delivered.name,
                                delivered.task.0,
                            )),
                            Ok(_) => {}
                            Err(error) => reply.line(LineKind::Rsp, format_args!("task {}: {}", id.0, error)),
                        }
                        return None;
                    }
                    self.foreground = None;
                }
                text
            }
        };
        let text = text.trim();
        let (name, args) = match text.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim()),
            None => (text, ""),
        };
        let Some(command) = commands().find(|command| command.name == name) else {
            if text.is_empty() {
                reply.rsp("type help for a list of commands");
            } else if let Some(target @ programs::Target::File(_)) = programs::find(self, name) {
                let (args, background) = programs::split_background(args);
                programs::start(self, target, args, background, reply);
            } else {
                reply.line(LineKind::Rsp, format_args!("unknown command '{name}', try help"));
            }
            return None;
        };
        match (command.run)(self, args, reply) {
            Outcome::Done => None,
            Outcome::Usage => {
                reply.line(LineKind::Rsp, format_args!("usage: {}", usage(command)));
                None
            }
            Outcome::Then(action) => Some(action),
        }
    }
}
