// storage.rs
//! The SD card and its filesystem.

use alloc::vec::Vec;
use rustypi_core::mbr::Volume;
use rustypi_core::session::{LineKind, Reply};
use super::{Command, Outcome, Shell};
use crate::sys;
use crate::sys::fs::EntryKind;

/// Most lines `ls` and `cat` reply with, so a reply stays a reasonable size.
const MAX_LISTING_LINES: usize = 40;

pub const COMMANDS: &[Command] = &[
    Command { name: "sd", args: "", description: "sd card and filesystem info", run: sd },
    Command { name: "ls", args: "[-a] [path]", description: "list a directory; -a shows dotfiles", run: ls },
    Command { name: "cat", args: "<path>", description: "show the start of a text file", run: cat },
];

fn sd<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    if !args.is_empty() {
        return Outcome::Usage;
    }
    let info = match sys::fs::info() {
        Ok(info) => info,
        Err(error) => {
            reply.line(LineKind::Rsp, format_args!("{}", error));
            return Outcome::Done;
        }
    };
    let location = match info.volume {
        Volume::Partition { index, partition } => {
            alloc::format!("partition {} at block {}", index + 1, partition.start.0)
        }
        Volume::WholeDisk => "whole card".into(),
    };
    reply.line(LineKind::Rsp, format_args!(
        "{:?} '{}', {}, {} KB clusters",
        info.fat_type,
        info.label,
        location,
        info.cluster_size / 1024,
    ));
    let kind = if info.high_capacity { "SDHC/SDXC" } else { "SDSC" };
    match info.card_blocks {
        Some(blocks) => reply.line(LineKind::Rsp, format_args!("card: {}, {} MB", kind, blocks / 2048)),
        None => reply.line(LineKind::Rsp, format_args!("card: {}", kind)),
    }
    Outcome::Done
}

/// Dotfiles (like the `._*` files macOS leaves on FAT volumes) are hidden unless `-a` is
/// given.
fn ls<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    let (all, path) = match args.strip_prefix("-a") {
        Some(rest) if rest.is_empty() || rest.starts_with(' ') => (true, rest.trim()),
        _ => (false, args),
    };
    if path.contains(char::is_whitespace) {
        return Outcome::Usage;
    }
    let path = if path.is_empty() { "/" } else { path };
    let entries = match sys::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            reply.line(LineKind::Rsp, format_args!("{}: {}", path, error));
            return Outcome::Done;
        }
    };
    let entries: Vec<_> = entries.into_iter().filter(|e| all || !e.name.starts_with('.')).collect();
    if entries.is_empty() {
        reply.rsp("(empty)");
    }
    for entry in entries.iter().take(MAX_LISTING_LINES) {
        match entry.kind {
            EntryKind::Directory => reply.line(LineKind::Rsp, format_args!("{:>9}  {}/", "", entry.name)),
            EntryKind::File => reply.line(LineKind::Rsp, format_args!("{:>9}  {}", entry.size, entry.name)),
        }
    }
    if entries.len() > MAX_LISTING_LINES {
        reply.line(LineKind::Rsp, format_args!("... {} more", entries.len() - MAX_LISTING_LINES));
    }
    Outcome::Done
}

fn cat<'a>(_: &mut Shell, path: &'a str, reply: &mut Reply) -> Outcome<'a> {
    if path.is_empty() {
        return Outcome::Usage;
    }
    let data = match sys::fs::read_file(path) {
        Ok(data) => data,
        Err(error) => {
            reply.line(LineKind::Rsp, format_args!("{}: {}", path, error));
            return Outcome::Done;
        }
    };
    let Ok(text) = core::str::from_utf8(&data) else {
        reply.line(LineKind::Rsp, format_args!("binary file, {} bytes", data.len()));
        return Outcome::Done;
    };
    let lines: Vec<&str> = text.lines().collect();
    for line in lines.iter().take(MAX_LISTING_LINES) {
        reply.rsp(line);
    }
    if lines.len() > MAX_LISTING_LINES {
        reply.line(LineKind::Rsp, format_args!("... {} more lines", lines.len() - MAX_LISTING_LINES));
    }
    Outcome::Done
}
