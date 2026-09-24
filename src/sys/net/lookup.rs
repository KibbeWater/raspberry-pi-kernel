// net/lookup.rs
//! Asking other hosts: DNS lookups (for commands, and for the time server's address), and
//! setting the clock by SNTP. Both run in the network task: they hand it datagrams to send
//! and take the answers that come back to their ports.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use rustypi_core::net::dns::{self, Answer};
use rustypi_core::net::{sntp, Ipv4};
use crate::println;
use crate::sys::clock;

/// Our UDP ports for DNS queries and SNTP requests.
pub const DNS_PORT: u16 = 49152;
pub const SNTP_PORT: u16 = 49153;

/// A DNS query goes again after this long without an answer, up to `DNS_TRIES` times.
const DNS_RETRY_US: u64 = 1_000_000;
const DNS_TRIES: u32 = 3;
/// The same for SNTP requests.
const SNTP_RETRY_US: u64 = 2_000_000;
const SNTP_TRIES: u32 = 3;
/// The clock is set again this often, and tried again this soon after failing.
const RESYNC_US: u64 = 3_600_000_000;
const RETRY_US: u64 = 30_000_000;

/// The time servers asked, by name: a pool that picks a nearby one.
const TIME_SERVERS: &str = "pool.ntp.org";
/// Asked instead when DHCP gave no DNS server: Cloudflare's time service (anycast).
const FALLBACK_TIME_SERVER: Ipv4 = Ipv4([162, 159, 200, 123]);

/// A datagram for the interface to send, from one of our ports.
pub struct Datagram {
    pub to: Ipv4,
    pub port: u16,
    pub from_port: u16,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// No network address yet.
    NoNetwork,
    /// DHCP gave no DNS server to ask.
    NoServer,
    BadName,
    NoSuchName,
    /// The server couldn't answer (its response code).
    Failed(u8),
    Timeout,
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ResolveError::NoNetwork => write!(f, "no network address yet"),
            ResolveError::NoServer => write!(f, "no DNS server"),
            ResolveError::BadName => write!(f, "not a name DNS can look up"),
            ResolveError::NoSuchName => write!(f, "no such name"),
            ResolveError::Failed(code) => write!(f, "the DNS server failed (code {code})"),
            ResolveError::Timeout => write!(f, "the DNS server didn't answer"),
        }
    }
}

/// A lookup's result: the name's addresses.
pub type Resolved = Result<Vec<Ipv4>, ResolveError>;

/// Who a lookup is for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// A command waiting for it, by request number.
    Command(u32),
    Clock,
}

struct Lookup {
    id: u16,
    query: Vec<u8>,
    server: Ipv4,
    purpose: Purpose,
    sent_at: u64,
    tries: u32,
}

/// Lookups in flight.
pub struct Resolver {
    next_id: u16,
    pending: Vec<Lookup>,
}

impl Resolver {
    pub fn new(seed: u16) -> Self {
        Resolver { next_id: seed, pending: Vec::new() }
    }

    /// Starts looking `name` up at `server`, sending the query into `out`.
    pub fn start(&mut self, name: &str, server: Ipv4, purpose: Purpose, now: u64, out: &mut Vec<Datagram>) -> Result<(), ResolveError> {
        self.next_id = self.next_id.wrapping_add(1);
        let query = dns::query(name, self.next_id).ok_or(ResolveError::BadName)?;
        out.push(Datagram { to: server, port: dns::PORT, from_port: DNS_PORT, data: query.clone() });
        self.pending.push(Lookup { id: self.next_id, query, server, purpose, sent_at: now, tries: 1 });
        Ok(())
    }

    /// Sends unanswered queries again, and gives up on those out of tries: they come back
    /// as timeouts.
    pub fn poll(&mut self, now: u64, out: &mut Vec<Datagram>) -> Vec<(Purpose, Resolved)> {
        let mut done = Vec::new();
        self.pending.retain_mut(|lookup| {
            if now < lookup.sent_at + DNS_RETRY_US {
                return true;
            }
            if lookup.tries >= DNS_TRIES {
                done.push((lookup.purpose, Err(ResolveError::Timeout)));
                return false;
            }
            lookup.tries += 1;
            lookup.sent_at = now;
            out.push(Datagram { to: lookup.server, port: dns::PORT, from_port: DNS_PORT, data: lookup.query.clone() });
            true
        });
        done
    }

    /// Takes a datagram that came to `DNS_PORT` from `from`: the answer to a lookup, if it is
    /// one.
    pub fn receive(&mut self, from: Ipv4, data: &[u8]) -> Option<(Purpose, Resolved)> {
        let (index, answer) = self
            .pending
            .iter()
            .enumerate()
            .filter(|(_, lookup)| lookup.server == from)
            .find_map(|(index, lookup)| dns::parse(data, lookup.id).map(|answer| (index, answer)))?;
        let lookup = self.pending.remove(index);
        let result = match answer {
            Answer::Addresses(addresses) if addresses.is_empty() => Err(ResolveError::NoSuchName),
            Answer::Addresses(addresses) => Ok(addresses),
            Answer::NoSuchName => Err(ResolveError::NoSuchName),
            Answer::Failed(code) => Err(ResolveError::Failed(code)),
        };
        Some((lookup.purpose, result))
    }
}

enum Sync {
    /// Until it is time to set the clock (again).
    Waiting { next_at: u64 },
    /// For the time servers' addresses.
    Resolving,
    /// For a time server's answer to the request with `cookie`.
    Asking { server: Ipv4, cookie: u64, sent_at: u64, tries: u32 },
}

/// Keeps the clock set: looks up the time servers, asks one, sets the clock, and does it
/// again every hour.
pub struct ClockSync {
    sync: Sync,
    set_once: bool,
}

impl ClockSync {
    pub const fn new() -> Self {
        ClockSync { sync: Sync::Waiting { next_at: 0 }, set_once: false }
    }

    /// Starts setting the clock if it is time to: `dns` is the DNS server, if there is one.
    pub fn poll(&mut self, now: u64, dns: Option<Ipv4>, resolver: &mut Resolver, out: &mut Vec<Datagram>) {
        match self.sync {
            Sync::Waiting { next_at } if now >= next_at => match dns {
                Some(server) => {
                    self.sync = Sync::Resolving;
                    if resolver.start(TIME_SERVERS, server, Purpose::Clock, now, out).is_err() {
                        self.sync = Sync::Waiting { next_at: now + RETRY_US };
                    }
                }
                None => self.ask(FALLBACK_TIME_SERVER, now, 1, out),
            },
            Sync::Asking { server, sent_at, tries, .. } if now >= sent_at + SNTP_RETRY_US => {
                if tries >= SNTP_TRIES {
                    self.sync = Sync::Waiting { next_at: now + RETRY_US };
                } else {
                    self.ask(server, now, tries + 1, out);
                }
            }
            _ => {}
        }
    }

    /// The time servers' addresses came back (or didn't).
    pub fn resolved(&mut self, result: Resolved, now: u64, out: &mut Vec<Datagram>) {
        match result.ok().and_then(|addresses| addresses.first().copied()) {
            Some(server) => self.ask(server, now, 1, out),
            None => self.sync = Sync::Waiting { next_at: now + RETRY_US },
        }
    }

    fn ask(&mut self, server: Ipv4, now: u64, tries: u32, out: &mut Vec<Datagram>) {
        // Nothing needs to guess it: it only tells our answer apart.
        let cookie = crate::sys::random::u64();
        out.push(Datagram { to: server, port: sntp::PORT, from_port: SNTP_PORT, data: sntp::request(cookie).to_vec() });
        self.sync = Sync::Asking { server, cookie, sent_at: now, tries };
    }

    /// Takes a datagram that came to `SNTP_PORT` from `from`: sets the clock if it is the
    /// answer we are waiting for.
    pub fn receive(&mut self, from: Ipv4, data: &[u8], now: u64) {
        let Sync::Asking { server, cookie, sent_at, .. } = self.sync else { return };
        if from != server {
            return;
        }
        let Some(unix_us) = sntp::parse(data, cookie, now - sent_at) else { return };
        clock::set(unix_us);
        if !self.set_once {
            self.set_once = true;
            if let Some((date, zone)) = clock::now() {
                println!("clock: {date} {zone}, from {server}");
            }
        }
        self.sync = Sync::Waiting { next_at: now + RESYNC_US };
    }
}

/// What a command asked to have looked up, and the answer, between it and the network task.
pub struct Request {
    pub number: u32,
    pub name: String,
}
