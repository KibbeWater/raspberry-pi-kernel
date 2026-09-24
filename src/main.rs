#![no_std]
#![no_main]
// Register maps list every offset, `+ 0x00` included.
#![allow(clippy::identity_op)]

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

use alloc::collections::VecDeque;
use alloc::string::String;
use core::sync::atomic::{AtomicU32, Ordering};
use core::time::Duration;
use commands::Shell;
use drivers::uart::Uart;
use link::{Link, Stats};
use rustypi_core::session::{self, Session};
use synchronization::{interface::Mutex, IrqLock};

/// Name announced in `HELLO` frames.
const NAME: &str = "RustyPI";

/// How often the Pi reports its receive counters, even when nothing talks to it.
const STAT_INTERVAL: Duration = Duration::from_secs(5);

/// The link's receive counters, published by the link task for the `stat` task.
static LINK_STATS: IrqLock<Stats> = IrqLock::new(Stats {
    rx_bytes: 0,
    rx_errors: 0,
    dropped: 0,
    bad_frames: 0,
    overflows: 0,
});

/// What the link task hands the shell task.
enum Inbound {
    /// The Uno (re)started: its sequence numbers start over.
    Hello,
    /// A `MSG` frame's payload.
    Msg(String),
}

/// Frames waiting for the shell task, oldest first.
static INBOX: IrqLock<VecDeque<Inbound>> = IrqLock::new(VecDeque::new());
/// More than this many waiting and new ones are dropped; the Uno retransmits.
const INBOX_LIMIT: usize = 8;
/// The sequence number of the request the shell has queued or is running, plus one; 0 for
/// none. A retransmit of it is answered with `BUSY`.
static PENDING: AtomicU32 = AtomicU32::new(0);

/// Called by `arch/boot.s` on core 0 at EL1, once the stack and `.bss` are set up.
#[no_mangle]
pub extern "C" fn kernel_main() -> ! {
    sys::init();
    sys::delay(Duration::from_secs(1));
    Uart::init(link::BAUD);
    if let Err(error) = sys::console::init() {
        println!("no screen: {}", error);
    }
    println!("Hello from RPi! {} {} at EL{}", NAME, sys::VERSION, sys::exception_level());
    match sys::memory::init() {
        Ok(bytes) => println!("memory: {} MB of pages for programs", bytes >> 20),
        Err(error) => println!("memory: no pages for programs: {}", error),
    }
    match sys::fs::mount() {
        Ok(info) => println!("sd: {:?} volume '{}' mounted", info.fat_type, info.label),
        Err(error) => println!("sd: {}", error),
    }
    link::send("HELLO", NAME);

    // From here on this is task 0, the shell.
    sched::init("shell");
    sched::spawn("link", link_task);
    sched::spawn("stat", stat_task);
    sys::enable_interrupts();
    shell_task()
}

/// Reads frames from the Arduino, sleeping until the UART receives something. Answers what
/// it can itself, so the link stays responsive while the shell runs a slow command, and
/// passes the rest to the shell task.
fn link_task() {
    let mut link = Link::new();
    loop {
        link.poll(|kind, payload| match kind {
            "PING" => link::send("PONG", payload),
            "HELLO" => {
                PENDING.store(0, Ordering::Relaxed);
                deliver(Inbound::Hello);
                link::send("HELLO", NAME);
            }
            "MSG" => match session::request_seq(payload) {
                // The shell has it already: tell the Uno to keep waiting.
                Some(seq) if PENDING.load(Ordering::Relaxed) == seq as u32 + 1 => {
                    link::send_fmt("BUSY", format_args!("{seq}"));
                }
                // Without a sequence number, the session rejects it. Marked pending before it
                // is queued, so the shell can't answer it first; a frame that didn't fit isn't
                // pending, so the Uno's retransmit gets another try.
                seq => {
                    if let Some(seq) = seq {
                        PENDING.store(seq as u32 + 1, Ordering::Relaxed);
                    }
                    if !deliver(Inbound::Msg(payload.into())) {
                        PENDING.store(0, Ordering::Relaxed);
                    }
                }
            },
            _ => link::send_fmt("ERR", format_args!("unknown kind {kind}")),
        });
        LINK_STATS.lock(|stats| *stats = *link.stats());
        sched::wait_until(sched::UART_RX, Uart::has_input);
    }
}

/// Queues `frame` for the shell task, false if the inbox is full and it was dropped.
fn deliver(frame: Inbound) -> bool {
    let queued = INBOX.lock(|inbox| {
        let room = inbox.len() < INBOX_LIMIT;
        if room {
            inbox.push_back(frame);
        }
        room
    });
    if queued {
        sched::notify(sched::SHELL_INBOX);
    }
    queued
}

/// Runs the commands the link task passes on, one at a time, and sends their replies.
fn shell_task() -> ! {
    let mut shell = Shell::new();
    let mut session = Session::new();
    loop {
        sched::wait_until(sched::SHELL_INBOX, || INBOX.lock(|inbox| !inbox.is_empty()));
        let Some(frame) = INBOX.lock(|inbox| inbox.pop_front()) else { continue };
        match frame {
            Inbound::Hello => session.reset(),
            Inbound::Msg(payload) => {
                let run = |text, reply: &mut _| shell.handle(text, reply);
                let action = session.handle(&payload, run, link::send_fmt);
                // Replied: a retransmit now gets the cached reply rather than BUSY. Unless the
                // Uno already sent its next request, which is pending now instead.
                if let Some(seq) = session::request_seq(&payload) {
                    let _ = PENDING.compare_exchange(seq as u32 + 1, 0, Ordering::Relaxed, Ordering::Relaxed);
                }
                if let Some(action) = action {
                    action.perform();
                }
            }
        }
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
