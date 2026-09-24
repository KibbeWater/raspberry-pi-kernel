// net.rs
//! Networking over the Pi 3 B+'s Ethernet: a task started at boot waits for USB to find the
//! LAN7800, brings it up with the board's MAC address, and runs a
//! `rustypi_core::net::interface::Interface` on it, which gets an address by DHCP and answers
//! ARP and pings. `ping` sends pings of our own.

use alloc::collections::VecDeque;
use core::fmt;
use core::time::Duration;
use rustypi_core::net::interface::{Config, Event, Interface};
use rustypi_core::net::{Ipv4, Mac};
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

pub fn status() -> Status {
    STATUS.lock(|status| *status)
}

/// Starts networking in the background.
pub fn start() {
    sched::spawn("net", run);
}

fn run() {
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

        let mut out = alloc::vec::Vec::new();
        while let Some((target, seq)) = PING_REQUESTS.lock(|requests| requests.pop_front()) {
            out.extend(interface.ping(target, seq, now));
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
                alloc::vec::Vec::new()
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
            }
        }
        // Straight on while frames keep coming; otherwise a tick's rest.
        if frames.is_empty() {
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
