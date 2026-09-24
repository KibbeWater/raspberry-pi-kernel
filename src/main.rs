#![no_std]
#![no_main]

mod board;
mod commands;
mod drivers;
mod link;
#[allow(dead_code)] // Not used yet; kept for when interrupts or cores come in.
mod synchronization;
mod sys;

use core::arch::global_asm;
use core::time::Duration;
use commands::Shell;
use drivers::uart::Uart;
use link::{Link, Stats};

global_asm!(include_str!("boot.s"));

/// Name announced in `HELLO` frames.
const NAME: &str = "RustyPI";

/// How often the Pi reports its receive counters, even when nothing talks to it.
const STAT_INTERVAL: Duration = Duration::from_secs(5);

/// Called by `boot.s` on core 0 once the stack and `.bss` are set up.
#[no_mangle]
pub extern "C" fn kernel_main() -> ! {
    sys::sleep(Duration::from_secs(1));
    Uart::init(link::BAUD);
    println!("Hello from RPi!");
    link::send("HELLO", NAME);

    let mut shell = Shell::new();

    // Poll continuously: the PL011 RX FIFO only holds 16 bytes (~4ms at 38400 baud),
    // so nothing in this loop may block.
    let mut link = Link::new();
    let mut last_stat = sys::uptime();
    loop {
        link.poll(|kind, payload| match kind {
            "PING" => link::send("PONG", payload),
            "HELLO" => link::send("HELLO", NAME),
            "MSG" => shell.handle(payload),
            _ => link::send_fmt("ERR", format_args!("unknown kind {}", kind)),
        });

        if sys::uptime() - last_stat >= STAT_INTERVAL {
            last_stat = sys::uptime();
            send_stat(&link.stats);
        }
    }
}

/// Sends a `STAT` frame so the Arduino can tell "Pi hung" apart from "Pi alive but
/// receiving garbage".
fn send_stat(stats: &Stats) {
    link::send_fmt("STAT", format_args!(
        "up={} rx={} err={} drop={} bad={} long={}",
        sys::uptime().as_secs(),
        stats.rx_bytes,
        stats.rx_errors,
        stats.dropped,
        stats.bad_frames,
        stats.overflows,
    ));
}
