// memory.rs
//! The heap.

use alloc::boxed::Box;
use alloc::vec::Vec;
use rustypi_core::session::{LineKind, Reply};
use super::{Command, Outcome, Shell};
use crate::sys;

pub const COMMANDS: &[Command] = &[
    Command { name: "heap", args: "[test]", description: "heap usage, or an allocator stress test", run: heap },
];

fn heap<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    match args {
        "" => usage(reply),
        "test" => stress_test(reply),
        _ => return Outcome::Usage,
    }
    Outcome::Done
}

fn usage(reply: &mut Reply) {
    let stats = sys::heap_stats();
    reply.line(LineKind::Rsp, format_args!(
        "heap {} of {} KB used, largest free block {} KB",
        stats.used / 1024,
        stats.total / 1024,
        stats.largest_free / 1024,
    ));
}

#[repr(align(256))]
struct Aligned(u8);

/// Allocates, checks and frees a spread of block sizes and alignments, then checks that
/// the heap is back where it started with no fragments left behind.
fn stress_test(reply: &mut Reply) {
    let before = sys::heap_stats();

    let mut blocks: Vec<Vec<u8>> = Vec::new();
    for i in 0..200usize {
        let size = 1 + (i * 37) % 3000;
        blocks.push((0..size).map(|j| (i + j) as u8).collect());
    }
    // Free every other block to leave holes, then fill them with differently sized blocks.
    for i in (0..blocks.len()).step_by(2) {
        blocks[i] = Vec::new();
    }
    for i in (0..blocks.len()).step_by(2) {
        let size = 1 + (i * 53) % 1500;
        blocks[i] = (0..size).map(|j| (i + j) as u8).collect();
    }
    let aligned: Vec<Box<Aligned>> = (0..16).map(|i| Box::new(Aligned(i))).collect();

    let corrupt = blocks.iter().enumerate().any(|(i, block)| {
        block.iter().enumerate().any(|(j, &byte)| byte != (i + j) as u8)
    }) || aligned.iter().enumerate().any(|(i, b)| b.0 != i as u8);
    let misaligned = aligned.iter().any(|b| (&**b as *const Aligned as usize) % 256 != 0);
    let peak = sys::heap_stats().used;
    drop(blocks);
    drop(aligned);
    let after = sys::heap_stats();

    let leaked = after.used != before.used || after.largest_free != before.largest_free;
    let ok = !corrupt && !misaligned && !leaked;
    reply.line(LineKind::Rsp, format_args!(
        "heap test {}: peak {} KB{}{}{}",
        if ok { "passed" } else { "FAILED" },
        peak / 1024,
        if corrupt { ", data corrupted" } else { "" },
        if misaligned { ", bad alignment" } else { "" },
        if leaked { ", memory not fully returned" } else { "" },
    ));
}
