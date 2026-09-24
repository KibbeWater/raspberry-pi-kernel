// net.rs
//! Networking over the Pi 3 B+'s Ethernet: a task started at boot waits for USB to find the
//! LAN7800, brings it up with the board's MAC address, and runs a
//! `rustypi_core::net::interface::Interface` on it, which gets an address by DHCP and answers
//! ARP and pings. `ping` sends pings of our own. Datagrams to UDP port 2323 are a console:
//! each line is a shell command, and `reply` sends the answer back. Whoever sent the last one
//! also gets a copy of everything printed (`mirror`), so programs work over it too.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use rustypi_core::net::interface::{Config, Event, Interface};
use rustypi_core::net::{Ipv4, Mac};
use rustypi_core::sched::TaskId;
use crate::drivers::lan7800::{self, Lan7800, Link};
use crate::drivers::mailbox::tags::GetMacAddress;
use crate::drivers::mailbox::{query, Mailbox};
use crate::drivers::timer;
use crate::synchronization::{interface::Mutex as _, IrqLock};
use crate::sys::usb::{self, Status as UsbStatus};
use crate::{println, sched};

/// How often the link is checked.
const LINK_CHECK: Duration = Duration::from_secs(1);
/// How long a quiet network makes the task sleep between polls: the scheduler's tick.
const IDLE_POLL: Duration = Duration::from_millis(10);
/// How long to wait for USB to find the LAN7800 before giving up.
const USB_WAIT: Duration = Duration::from_secs(20);

/// The UDP port of the console.
pub const CONSOLE_PORT: u16 = 2323;
/// Most bytes of console output in one datagram. Inside a frame, but mostly inside what
/// macOS `nc -u` reads of a datagram (1024 bytes: it drops the rest).
const MAX_REPLY_DATAGRAM: usize = 1024;
/// Console datagrams waiting to go out, at most: past it, the oldest are dropped.
const MAX_CONSOLE_QUEUE: usize = 64;

/// Who sent a console line, and so where its answer goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Peer {
    pub address: Ipv4,
    pub port: u16,
}

/// Where networking is, for `net`.
#[derive(Clone, Copy)]
pub enum State {
    Starting,
    /// No Ethernet controller: USB failed, or found none.
    NoController,
    Failed,
    Running,
}

/// What `status` reports.
#[derive(Clone, Copy)]
pub struct Status {
    pub state: State,
    pub mac: Option<Mac>,
    pub link: Option<Link>,
    pub config: Option<Config>,
}

static STATUS: IrqLock<Status> = IrqLock::new(Status { state: State::Starting, mac: None, link: None, config: None });
/// Pings `ping` asked for, and the answers the task got.
static PING_REQUESTS: IrqLock<VecDeque<(Ipv4, u16)>> = IrqLock::new(VecDeque::new());
static PING_REPLIES: IrqLock<VecDeque<(Ipv4, u16, u64)>> = IrqLock::new(VecDeque::new());
/// Console replies waiting to go out.
static CONSOLE_OUT: IrqLock<VecDeque<(Peer, Vec<u8>)>> = IrqLock::new(VecDeque::new());
/// The network task, plus one (0 until it starts), to wake when there is something to send.
static TASK: AtomicUsize = AtomicUsize::new(0);
/// The console's user: whoever sent the last line, who gets a copy of what is printed.
static ATTACHED: IrqLock<Option<Peer>> = IrqLock::new(None);

/// Queues console output for `peer`.
fn queue(peer: Peer, data: &[u8]) {
    CONSOLE_OUT.lock(|out| {
        out.push_back((peer, data.to_vec()));
        while out.len() > MAX_CONSOLE_QUEUE {
            out.pop_front();
        }
    });
}

/// Sends a copy of something printed to the console's user, if there is one. Not from
/// interrupt handlers, a panic, or the network task itself (whose own complaints about
/// sending would otherwise send more).
pub fn mirror(args: fmt::Arguments) {
    if !crate::arch::irqs_enabled() || super::panicking() {
        return;
    }
    let Some(peer) = ATTACHED.lock(|attached| *attached) else { return };
    if TASK.load(Ordering::Relaxed).checked_sub(1) == Some(sched::current().0) {
        return;
    }
    let text = alloc::format!("{args}");
    for chunk in text.as_bytes().chunks(MAX_REPLY_DATAGRAM) {
        queue(peer, chunk);
    }
    wake();
}

/// Cuts the network task's sleep short, for something to send now.
fn wake() {
    if let Some(id) = TASK.load(Ordering::Relaxed).checked_sub(1) {
        sched::interrupt(TaskId(id));
    }
}

pub fn status() -> Status {
    STATUS.lock(|status| *status)
}

/// Starts networking in the background. Console lines, and who sent them, go to
/// `on_console`.
pub fn start(on_console: fn(String, Peer)) {
    let id = sched::spawn("net", move || run(on_console));
    TASK.store(id.0 + 1, Ordering::Relaxed);
}

/// Sends `text`, a console answer, to `peer`, in as many datagrams as it takes (split
/// between lines where it can).
pub fn reply(peer: Peer, text: &str) {
    let mut rest = text.as_bytes();
    while !rest.is_empty() {
        let mut end = rest.len().min(MAX_REPLY_DATAGRAM);
        if end < rest.len() {
            if let Some(newline) = rest[..end].iter().rposition(|&b| b == b'\n') {
                end = newline + 1;
            }
        }
        queue(peer, &rest[..end]);
        rest = &rest[end..];
    }
    wake();
}

fn run(on_console: fn(String, Peer)) {
    let Some(mut lan) = bring_up() else { return };
    let mut interface = Interface::new(lan.mac, super::random::u64() as u32);
    STATUS.lock(|status| {
        status.state = State::Running;
        status.mac = Some(lan.mac);
    });
    let mut next_link_check = 0;
    let mut link_up = false;
    let mut failing = false;
    loop {
        let now = timer::now_us();
        if now >= next_link_check {
            next_link_check = now + LINK_CHECK.as_micros() as u64;
            if let Some(Ok(link)) = usb::with_bus(|host, _| lan.link(host)) {
                if link.up != link_up {
                    link_up = link.up;
                    match link.speed {
                        Some((mbps, full)) => println!("net: link up, {mbps} Mbps {} duplex", if full { "full" } else { "half" }),
                        None if link.up => println!("net: link up"),
                        None => println!("net: link down"),
                    }
                }
                STATUS.lock(|status| status.link = Some(link));
            }
        }

        let mut out = Vec::new();
        while let Some((target, seq)) = PING_REQUESTS.lock(|requests| requests.pop_front()) {
            out.extend(interface.ping(target, seq, now));
        }
        while let Some((peer, data)) = CONSOLE_OUT.lock(|replies| replies.pop_front()) {
            out.extend(interface.send_udp(peer.address, peer.port, CONSOLE_PORT, &data, now));
        }
        let frames = match usb::with_bus(|host, _| lan.receive(host)) {
            Some(Ok(frames)) => {
                failing = false;
                frames
            }
            Some(Err(error)) => {
                if !failing {
                    println!("net: receiving: {error}");
                    failing = true;
                }
                Vec::new()
            }
            None => return,
        };
        for frame in &frames {
            out.extend(interface.receive(frame, now));
        }
        out.extend(interface.poll(now));
        if link_up {
            for frame in out {
                if let Some(Err(error)) = usb::with_bus(|host, _| lan.send(host, &frame)) {
                    println!("net: sending: {error}");
                }
            }
        }
        for event in interface.take_events() {
            match event {
                Event::Configured(config) => {
                    println!("net: address {} from DHCP", Address(config));
                    STATUS.lock(|status| status.config = Some(config));
                }
                Event::Unconfigured => {
                    println!("net: address lost");
                    STATUS.lock(|status| status.config = None);
                }
                Event::PingReply { from, seq, rtt_us } => PING_REPLIES.lock(|replies| {
                    replies.push_back((from, seq, rtt_us));
                    // Answers nobody waits for any more don't pile up.
                    while replies.len() > 16 {
                        replies.pop_front();
                    }
                }),
                Event::Udp { from, from_port, port: CONSOLE_PORT, data } => {
                    let text = String::from_utf8_lossy(&data).into_owned();
                    let peer = Peer { address: from, port: from_port };
                    ATTACHED.lock(|attached| *attached = Some(peer));
                    on_console(text, peer);
                }
                Event::Udp { .. } => {}
            }
        }
        // Straight on while frames keep coming or an answer is due (so round trips are
        // measured, not rounded up to ticks); otherwise a tick's rest, unless woken.
        if frames.is_empty() && !interface.awaiting() {
            sched::sleep(IDLE_POLL);
        } else {
            sched::yield_now();
        }
    }
}

/// Waits for USB to enumerate, then brings up the LAN7800 on it.
fn bring_up() -> Option<Lan7800> {
    let fail = |state| {
        STATUS.lock(|status| status.state = state);
        None
    };
    let mac = match query::<GetMacAddress>(&Mailbox, ()) {
        Ok(mac) => Mac(mac.bytes),
        Err(error) => {
            println!("net: no MAC address: {error}");
            return fail(State::Failed);
        }
    };
    let started = timer::now_us();
    loop {
        if usb::inspect(|status| matches!(status, UsbStatus::Failed(_))) {
            return fail(State::NoController);
        }
        let found = usb::with_bus(|host, tree| {
            let device = tree
                .root
                .walk()
                .into_iter()
                .map(|(_, device)| device)
                .find(|d| d.descriptor.vendor == lan7800::VENDOR && d.descriptor.product == lan7800::PRODUCT)?;
            Some(Lan7800::start(host, device, mac))
        });
        match found {
            Some(Some(Ok(lan))) => {
                println!("net: LAN7800 up, MAC {}", lan.mac);
                return Some(lan);
            }
            Some(Some(Err(error))) => {
                println!("net: LAN7800: {error}");
                return fail(State::Failed);
            }
            Some(None) => {
                println!("net: no Ethernet controller on the USB bus");
                return fail(State::NoController);
            }
            // USB is still starting.
            None => {}
        }
        if timer::now_us() - started > USB_WAIT.as_micros() as u64 {
            return fail(State::NoController);
        }
        sched::sleep(Duration::from_millis(100));
    }
}

#[derive(Debug)]
pub enum PingError {
    /// No address yet (or no network).
    NoAddress,
    Timeout,
}

impl fmt::Display for PingError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PingError::NoAddress => write!(f, "no network address yet"),
            PingError::Timeout => write!(f, "timed out"),
        }
    }
}

/// Pings `target` once, numbered `seq`, and waits up to `timeout` for the answer. Returns
/// the round trip, in microseconds.
pub fn ping(target: Ipv4, seq: u16, timeout: Duration) -> Result<u64, PingError> {
    if STATUS.lock(|status| status.config.is_none()) {
        return Err(PingError::NoAddress);
    }
    PING_REPLIES.lock(|replies| replies.retain(|&(_, pending, _)| pending != seq));
    PING_REQUESTS.lock(|requests| requests.push_back((target, seq)));
    wake();
    let started = timer::now_us();
    loop {
        let answer = PING_REPLIES.lock(|replies| {
            let index = replies.iter().position(|&(from, pending, _)| pending == seq && from == target)?;
            replies.remove(index)
        });
        if let Some((_, _, rtt_us)) = answer {
            return Ok(rtt_us);
        }
        if timer::now_us() - started > timeout.as_micros() as u64 {
            return Err(PingError::Timeout);
        }
        sched::sleep(Duration::from_millis(1));
    }
}

/// An address with its netmask as a prefix length, and its gateway: `192.168.1.50/24 via
/// 192.168.1.1`.
pub struct Address(pub Config);

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let prefix = u32::from_be_bytes(self.0.netmask.0).leading_ones();
        write!(f, "{}/{}", self.0.address, prefix)?;
        if let Some(gateway) = self.0.gateway {
            write!(f, " via {gateway}")?;
        }
        Ok(())
    }
}
