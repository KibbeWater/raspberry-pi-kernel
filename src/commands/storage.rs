// storage.rs
//! The SD card and its filesystem.

use alloc::vec::Vec;
use rustypi_abi::MAX_FILE;
use rustypi_core::mbr::Volume;
use rustypi_core::session::{LineKind, Reply};
use super::{Command, Outcome, Shell};
use crate::sys;
use crate::sys::fs::EntryKind;

/// Most lines `ls` and `cat` reply with, so a reply stays a reasonable size.
const MAX_LISTING_LINES: usize = 40;

pub const COMMANDS: &[Command] = &[
    Command { name: "sd", args: "[bench|writetest]", description: "sd card info, a read speed test, or a write test", run: sd },
    Command { name: "ls", args: "[-a] [path]", description: "list a directory; -a shows dotfiles", run: ls },
    Command { name: "cat", args: "<path>", description: "show the start of a text file", run: cat },
    Command { name: "write", args: "<path> <text>", description: "write a line of text to a file", run: write },
    Command { name: "rm", args: "<path>", description: "remove a file or empty directory", run: rm },
    Command { name: "mkdir", args: "<path>", description: "make a directory", run: mkdir },
];

fn write<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    let Some((path, text)) = args.split_once(char::is_whitespace) else {
        return Outcome::Usage;
    };
    let mut line = text.trim_start().as_bytes().to_vec();
    line.push(b'\n');
    match sys::fs::write_file(path, &line) {
        Ok(()) => reply.line(LineKind::Rsp, format_args!("wrote {} bytes to {}", line.len(), path)),
        Err(error) => reply.line(LineKind::Rsp, format_args!("write: {path}: {error}")),
    }
    Outcome::Done
}

fn rm<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    if args.is_empty() || args.contains(char::is_whitespace) {
        return Outcome::Usage;
    }
    if let Err(error) = sys::fs::remove(args) {
        reply.line(LineKind::Rsp, format_args!("rm: {args}: {error}"));
    }
    Outcome::Done
}

fn mkdir<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    if args.is_empty() || args.contains(char::is_whitespace) {
        return Outcome::Usage;
    }
    if let Err(error) = sys::fs::create_dir(args) {
        reply.line(LineKind::Rsp, format_args!("mkdir: {args}: {error}"));
    }
    Outcome::Done
}

fn sd<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    match args {
        "" => {}
        "bench" => {
            bench(reply);
            return Outcome::Done;
        }
        "writetest" => {
            match sys::fs::write_test() {
                Ok((true, true)) => reply.rsp("write test passed: single and multi-block writes read back, old contents restored"),
                Ok((patterns, restored)) => reply.line(LineKind::Rsp, format_args!(
                    "write test FAILED: patterns {}, restore {}",
                    if patterns { "ok" } else { "wrong" },
                    if restored { "ok" } else { "wrong" },
                )),
                Err(error) => reply.line(LineKind::Rsp, format_args!("write test: {error}")),
            }
            return Outcome::Done;
        }
        _ => return Outcome::Usage,
    }
    let info = match sys::fs::info() {
        Ok(info) => info,
        Err(error) => {
            reply.line(LineKind::Rsp, format_args!("{error}"));
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
    let (width, hz) = info.bus;
    match info.card_blocks {
        Some(blocks) => reply.line(LineKind::Rsp, format_args!(
            "card: {}, {} MB, {}-bit bus at {} MHz",
            kind,
            blocks / 2048,
            width,
            hz / 1_000_000,
        )),
        None => reply.line(LineKind::Rsp, format_args!("card: {}, {}-bit bus at {} MHz", kind, width, hz / 1_000_000)),
    }
    match sys::fs::free_space() {
        Ok(bytes) => reply.line(LineKind::Rsp, format_args!("free: {} MB", bytes / (1024 * 1024))),
        Err(error) => reply.line(LineKind::Rsp, format_args!("free: {error}")),
    }
    Outcome::Done
}

/// Times reading the biggest file in the root directory that the kernel reads whole.
fn bench(reply: &mut Reply) {
    let biggest = sys::fs::read_dir("/").ok().and_then(|entries| {
        entries
            .into_iter()
            .filter(|entry| entry.kind == EntryKind::File && entry.size as usize <= MAX_FILE)
            .max_by_key(|entry| entry.size)
    });
    let Some(file) = biggest else {
        reply.rsp("sd bench: no file to read");
        return;
    };
    let start = sys::uptime();
    let result = sys::fs::read_file(&file.name);
    let micros = (sys::uptime() - start).as_micros().max(1) as u64;
    match result {
        Ok(bytes) => reply.line(LineKind::Rsp, format_args!(
            "read {} ({} KB) in {} ms: {} KB/s",
            file.name,
            bytes.len() / 1024,
            micros / 1000,
            bytes.len() as u64 * 1_000_000 / 1024 / micros,
        )),
        Err(error) => reply.line(LineKind::Rsp, format_args!("sd bench: {}: {}", file.name, error)),
    }
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
            reply.line(LineKind::Rsp, format_args!("{path}: {error}"));
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
            reply.line(LineKind::Rsp, format_args!("{path}: {error}"));
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
