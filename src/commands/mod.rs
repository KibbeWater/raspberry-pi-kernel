// commands/mod.rs
//! Commands typed on the host, delivered by the Arduino bridge as `MSG` frames.
//!
//! Every command is a `Command` in one of the `GROUPS` tables. `help` and the dispatcher
//! both read those tables, so the help text can't drift from what is actually accepted.
//! Handlers write their answer into a `session::Reply`.

mod led;
mod memory;
mod screen;
mod storage;
mod system;
mod tasks;

use alloc::format;
use rustypi_core::session::{LineKind, Reply};

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
];

fn commands() -> impl Iterator<Item = &'static Command> {
    GROUPS.iter().flat_map(|group| group.iter())
}

fn usage(command: &Command) -> alloc::string::String {
    if command.args.is_empty() {
        command.name.into()
    } else {
        format!("{} {}", command.name, command.args)
    }
}

/// State that commands keep between calls.
pub struct Shell {
    led: led::Led,
}

impl Shell {
    pub fn new() -> Self {
        Shell { led: led::Led::new() }
    }

    /// Runs one command line. Returns an action to perform after the reply is sent.
    pub fn handle<'a>(&mut self, text: &'a str, reply: &mut Reply) -> Option<Action<'a>> {
        let text = text.trim();
        let (name, args) = match text.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim()),
            None => (text, ""),
        };
        let Some(command) = commands().find(|command| command.name == name) else {
            if text.is_empty() {
                reply.rsp("type help for a list of commands");
            } else {
                reply.line(LineKind::Rsp, format_args!("unknown command '{}', try help", name));
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
