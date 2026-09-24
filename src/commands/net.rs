// net.rs
//! Networking.

use core::time::Duration;
use rustypi_core::net::Ipv4;
use rustypi_core::session::{LineKind, Reply};
use super::{Command, Outcome, Shell};
use crate::sys;
use crate::sys::net::{Address, State};

pub const COMMANDS: &[Command] = &[
    Command { name: "net", args: "", description: "Ethernet link, MAC and IP address", run: net },
    Command { name: "ping", args: "<address> [count]", description: "ping an IPv4 address (4 times unless told)", run: ping },
    Command { name: "update", args: "", description: "take a new kernel over the network for a minute (tools/deploy.py)", run: update },
];

fn update<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    if !args.is_empty() {
        return Outcome::Usage;
    }
    sys::update::arm();
    reply.line(LineKind::Rsp, format_args!(
        "update: ready for {} s on UDP port {}",
        sys::update::ARMED_FOR.as_secs(),
        rustypi_core::update::PORT,
    ));
    Outcome::Done
}

/// Most pings one command sends: each takes up to a second, and the shell waits.
const MAX_PINGS: u16 = 20;
const PING_TIMEOUT: Duration = Duration::from_secs(1);

fn net<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    if !args.is_empty() {
        return Outcome::Usage;
    }
    let status = sys::net::status();
    match status.state {
        State::Starting => reply.rsp("net: starting"),
        State::NoController => reply.rsp("net: no Ethernet controller"),
        State::Failed => reply.rsp("net: the Ethernet controller didn't start"),
        State::Running => {}
    }
    if let Some(mac) = status.mac {
        reply.line(LineKind::Rsp, format_args!("mac: {mac}"));
    }
    match status.link {
        Some(link) if link.up => match link.speed {
            Some((mbps, full)) => reply.line(LineKind::Rsp, format_args!(
                "link: up, {} Mbps {} duplex",
                mbps,
                if full { "full" } else { "half" },
            )),
            None => reply.rsp("link: up"),
        },
        Some(_) => reply.rsp("link: down (is the cable in?)"),
        None => {}
    }
    if matches!(status.state, State::Running) {
        match status.config {
            Some(config) => {
                reply.line(LineKind::Rsp, format_args!("address: {}", Address(config)));
                if let Some(dns) = config.dns {
                    reply.line(LineKind::Rsp, format_args!("dns: {dns}"));
                }
            }
            None => reply.rsp("address: waiting for DHCP"),
        }
    }
    Outcome::Done
}

fn ping<'a>(_: &mut Shell, args: &'a str, reply: &mut Reply) -> Outcome<'a> {
    let mut words = args.split_whitespace();
    let Some(target) = words.next().and_then(Ipv4::parse) else {
        return Outcome::Usage;
    };
    let count = match words.next().map(str::parse::<u16>) {
        None => 4,
        Some(Ok(count)) if (1..=MAX_PINGS).contains(&count) => count,
        Some(_) => return Outcome::Usage,
    };
    if words.next().is_some() {
        return Outcome::Usage;
    }
    let mut received = 0;
    for seq in 1..=count {
        let started = sys::uptime();
        match sys::net::ping(target, seq, PING_TIMEOUT) {
            Ok(rtt_us) => {
                received += 1;
                reply.line(LineKind::Rsp, format_args!(
                    "reply from {}: seq={} time={}.{} ms",
                    target,
                    seq,
                    rtt_us / 1000,
                    rtt_us % 1000 / 100,
                ));
            }
            Err(error) => {
                reply.line(LineKind::Rsp, format_args!("seq={seq}: {error}"));
                if matches!(error, sys::net::PingError::NoAddress) {
                    return Outcome::Done;
                }
            }
        }
        // A ping a second, like the usual tool.
        if seq < count {
            if let Some(rest) = Duration::from_secs(1).checked_sub(sys::uptime() - started) {
                crate::sched::sleep(rest);
            }
        }
    }
    reply.line(LineKind::Rsp, format_args!("{count} sent, {received} received"));
    Outcome::Done
}
