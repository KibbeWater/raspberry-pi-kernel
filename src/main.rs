#![no_std]
#![no_main]

extern crate alloc;

mod arch;
mod board;
mod commands;
mod drivers;
mod link;
mod synchronization;
mod sys;

use core::time::Duration;
use commands::Shell;
use drivers::uart::Uart;
use link::{Link, Stats};
use rustypi_core::session::Session;

/// Name announced in `HELLO` frames.
const NAME: &str = "RustyPI";

/// How often the Pi reports its receive counters, even when nothing talks to it.
const STAT_INTERVAL: Duration = Duration::from_secs(5);

/// Called by `arch/boot.s` on core 0 at EL1, once the stack and `.bss` are set up.
#[no_mangle]
pub extern "C" fn kernel_main() -> ! {
    sys::init();
    sys::sleep(Duration::from_secs(1));
    Uart::init(link::BAUD);
    if let Err(error) = sys::console::init() {
        println!("no screen: {}", error);
    }
    println!("Hello from RPi! {} {} at EL{}", NAME, sys::VERSION, sys::exception_level());
    link::send("HELLO", NAME);

    let mut shell = Shell::new();

    // Received bytes are queued by the UART interrupt, so the loop can sleep between
    // wake-ups (UART input or the timer tick) without losing any.
    let mut link = Link::new();
    let mut session = Session::new();
    sys::enable_interrupts();
    let mut last_stat = sys::uptime();
    loop {
        link.poll(|kind, payload| match kind {
            "PING" => link::send("PONG", payload),
            "HELLO" => {
                session.reset();
                link::send("HELLO", NAME);
            }
            "MSG" => {
                let run = |text, reply: &mut _| shell.handle(text, reply);
                if let Some(action) = session.handle(payload, run, link::send_fmt) {
                    action.perform();
                }
            }
            _ => link::send_fmt("ERR", format_args!("unknown kind {}", kind)),
        });

        if sys::uptime() - last_stat >= STAT_INTERVAL {
            last_stat = sys::uptime();
            send_stat(link.stats());
        }

        sys::idle();
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
