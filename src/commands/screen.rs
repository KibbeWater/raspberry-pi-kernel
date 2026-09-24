// screen.rs
//! The HDMI screen console.

use rustypi_core::session::{LineKind, Reply};
use super::{Command, Outcome, Shell};
use crate::sys;

pub const COMMANDS: &[Command] = &[
    Command { name: "screen", args: "[test|redraw]", description: "screen info, colour bars, or redraw", run: screen },
];

fn screen<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    match args {
        "" => info(reply),
        "test" => {
            let drawn = sys::console::test_pattern();
            reply.rsp(if drawn { "bars from left: red, green, blue, white" } else { "no screen" });
        }
        "redraw" => reply.rsp(if sys::console::redraw() { "redrawn" } else { "no screen" }),
        _ => return Outcome::Usage,
    }
    Outcome::Done
}

fn info(reply: &mut Reply) {
    let Some(info) = sys::console::info() else {
        reply.rsp("no screen");
        return;
    };
    reply.line(LineKind::Rsp, format_args!(
        "{}x{}, pitch {}, {}, {}x{} text",
        info.width,
        info.height,
        info.pitch,
        if info.rgb { "rgb" } else { "bgr" },
        info.cols,
        info.rows,
    ));
    reply.line(LineKind::Rsp, format_args!(
        "framebuffer {:#x}..{:#x}",
        info.address,
        info.address + info.bytes,
    ));
}
