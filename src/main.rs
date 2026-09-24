#![no_std]
#![no_main]

extern crate alloc;

mod arch;
mod board;
mod commands;
mod drivers;
mod link;
mod process;
mod sched;
mod synchronization;
mod sys;

use core::time::Duration;
use commands::Shell;
use drivers::uart::Uart;
use link::{Link, Stats};
use rustypi_core::session::Session;
use synchronization::{interface::Mutex, IrqLock};

/// Name announced in `HELLO` frames.
const NAME: &str = "RustyPI";

/// How often the Pi reports its receive counters, even when nothing talks to it.
const STAT_INTERVAL: Duration = Duration::from_secs(5);

/// The link's receive counters, published by the shell task for the `stat` task.
static LINK_STATS: IrqLock<Stats> = IrqLock::new(Stats {
    rx_bytes: 0,
    rx_errors: 0,
    dropped: 0,
    bad_frames: 0,
    overflows: 0,
});

/// Called by `arch/boot.s` on core 0 at EL1, once the stack and `.bss` are set up.
#[no_mangle]
pub extern "C" fn kernel_main() -> ! {
    sys::init();
    process::init();
    sys::delay(Duration::from_secs(1));
    Uart::init(link::BAUD);
    if let Err(error) = sys::console::init() {
        println!("no screen: {}", error);
    }
    println!("Hello from RPi! {} {} at EL{}", NAME, sys::VERSION, sys::exception_level());
    match sys::fs::mount() {
        Ok(info) => println!("sd: {:?} volume '{}' mounted", info.fat_type, info.label),
        Err(error) => println!("sd: {}", error),
    }
    link::send("HELLO", NAME);

    // From here on this is task 0, the shell.
    sched::init("shell");
    sched::spawn("stat", stat_task);
    sys::enable_interrupts();
    shell_task()
}

/// Answers frames from the Arduino, sleeping until the UART receives something.
fn shell_task() -> ! {
    let mut shell = Shell::new();
    let mut link = Link::new();
    let mut session = Session::new();
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
        LINK_STATS.lock(|stats| *stats = *link.stats());
        sched::wait_until(sched::UART_RX, Uart::has_input);
    }
}

/// Sends a `STAT` frame every `STAT_INTERVAL`, so the Arduino can tell "Pi hung" apart from
/// "Pi alive but receiving garbage".
fn stat_task() {
    loop {
        sched::sleep(STAT_INTERVAL);
        let stats = LINK_STATS.lock(|stats| *stats);
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
}
