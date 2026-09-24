// net/interface.rs
//! One network interface: its addresses, and what it does with the frames it receives. It
//! answers ARP and pings, resolves neighbours with ARP before sending to them, sends pings of
//! its own, and gets its address by DHCP. Frames go in through `receive` and out as the
//! return values; `poll` runs its timers. Times are microseconds, from whatever clock the
//! caller keeps.

use alloc::vec::Vec;
use super::arp::{self, Arp};
use super::icmp::{self, Echo};
use super::{dhcp, ethernet, ipv4, udp, Ipv4, Mac};

/// What DHCP gave: the interface's address and how to reach the rest of the world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub address: Ipv4,
    pub netmask: Ipv4,
    pub gateway: Option<Ipv4>,
    pub dns: Option<Ipv4>,
    pub lease_secs: u32,
}

/// Something worth telling the kernel about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// DHCP gave the interface an address (or a new one).
    Configured(Config),
    /// DHCP took it back (the server said no to renewing it).
    Unconfigured,
    /// An answer to one of our pings.
    PingReply { from: Ipv4, seq: u16, rtt_us: u64 },
}

/// How often a DHCP message goes again without an answer, and how many REQUESTs go before
/// starting over with a DISCOVER.
const DHCP_RETRY_US: u64 = 4_000_000;
const DHCP_REQUEST_TRIES: u32 = 3;
/// A lease is renewed halfway through, but not more often than this.
const MIN_RENEW_US: u64 = 60_000_000;
/// ARP requests go again this often while a packet waits, until it is given up on.
const ARP_RETRY_US: u64 = 1_000_000;
const ARP_GIVE_UP_US: u64 = 3_000_000;
/// Neighbours remembered.
const ARP_CACHE: usize = 16;
/// Our pings older than this are forgotten.
const PING_TIMEOUT_US: u64 = 10_000_000;

enum Dhcp {
    Discovering { xid: u32, next_try: u64 },
    Requesting { xid: u32, offered: Ipv4, server: Ipv4, next_try: u64, tries: u32 },
    Bound { server: Ipv4, renew_at: u64 },
}

/// A packet waiting for its next hop's MAC address.
struct Waiting {
    next_hop: Ipv4,
    packet: Vec<u8>,
    queued_at: u64,
    asked_at: u64,
}

pub struct Interface {
    mac: Mac,
    config: Option<Config>,
    dhcp: Dhcp,
    /// Neighbours, newest last.
    arp: Vec<(Ipv4, Mac)>,
    waiting: Vec<Waiting>,
    /// IPv4 identification of the next packet sent.
    next_id: u16,
    /// Identifies our pings; each has its sequence number and when it went.
    ping_id: u16,
    pings: Vec<(u16, u64)>,
    events: Vec<Event>,
    /// For DHCP transaction IDs: a simple generator seeded by the caller.
    random: u32,
}

impl Interface {
    /// An interface with MAC address `mac`, which starts asking DHCP for an address at its
    /// first `poll`. `seed` makes its DHCP transactions and pings its own.
    pub fn new(mac: Mac, seed: u32) -> Self {
        Interface {
            mac,
            config: None,
            dhcp: Dhcp::Discovering { xid: seed, next_try: 0 },
            arp: Vec::new(),
            waiting: Vec::new(),
            next_id: seed as u16,
            ping_id: (seed >> 16) as u16,
            pings: Vec::new(),
            events: Vec::new(),
            random: seed | 1,
        }
    }

    pub fn mac(&self) -> Mac {
        self.mac
    }

    pub fn config(&self) -> Option<Config> {
        self.config
    }

    /// What happened since the last call.
    pub fn take_events(&mut self) -> Vec<Event> {
        core::mem::take(&mut self.events)
    }

    fn next_xid(&mut self) -> u32 {
        // xorshift32: plenty for telling transactions apart.
        self.random ^= self.random << 13;
        self.random ^= self.random >> 17;
        self.random ^= self.random << 5;
        self.random
    }

    /// Runs the timers: DHCP messages and ARP requests that are due again, packets given up
    /// on, pings forgotten. Returns frames to send.
    pub fn poll(&mut self, now: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        match self.dhcp {
            Dhcp::Discovering { xid, next_try } if now >= next_try => {
                self.dhcp = Dhcp::Discovering { xid, next_try: now + DHCP_RETRY_US };
                out.push(self.dhcp_frame(dhcp::discover(self.mac, xid)));
            }
            Dhcp::Requesting { xid, offered, server, next_try, tries } if now >= next_try => {
                if tries >= DHCP_REQUEST_TRIES {
                    let xid = self.next_xid();
                    self.dhcp = Dhcp::Discovering { xid, next_try: now };
                    return self.poll(now);
                }
                self.dhcp = Dhcp::Requesting { xid, offered, server, next_try: now + DHCP_RETRY_US, tries: tries + 1 };
                out.push(self.dhcp_frame(dhcp::request(self.mac, xid, offered, server)));
            }
            Dhcp::Bound { server, renew_at } if now >= renew_at => {
                let offered = self.config.map_or(Ipv4::UNSPECIFIED, |config| config.address);
                let xid = self.next_xid();
                self.dhcp = Dhcp::Requesting { xid, offered, server, next_try: now, tries: 0 };
                return self.poll(now);
            }
            _ => {}
        }

        self.waiting.retain(|waiting| now < waiting.queued_at + ARP_GIVE_UP_US);
        let mut ask = Vec::new();
        for waiting in &mut self.waiting {
            if now >= waiting.asked_at + ARP_RETRY_US {
                waiting.asked_at = now;
                ask.push(waiting.next_hop);
            }
        }
        for ip in ask {
            if let Some(frame) = self.arp_request(ip) {
                out.push(frame);
            }
        }
        self.pings.retain(|&(_, sent)| now < sent + PING_TIMEOUT_US);
        out
    }

    /// Sends a ping to `target`, number `seq`. Returns frames to send: the ping, or an ARP
    /// request first. Nothing without an address of our own, or a way to `target`.
    pub fn ping(&mut self, target: Ipv4, seq: u16, now: u64) -> Vec<Vec<u8>> {
        let data: Vec<u8> = (0..32u8).collect();
        let echo = Echo { kind: icmp::ECHO_REQUEST, id: self.ping_id, seq, data: &data }.to_bytes();
        self.pings.retain(|&(pending, _)| pending != seq);
        self.pings.push((seq, now));
        let mut out = Vec::new();
        self.send_ip(target, ipv4::ICMP, &echo, now, &mut out);
        out
    }

    /// Takes a frame that arrived. Returns frames to send in answer.
    pub fn receive(&mut self, bytes: &[u8], now: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let Some(frame) = ethernet::parse(bytes) else { return out };
        if frame.dst != self.mac && frame.dst != Mac::BROADCAST {
            return out;
        }
        match frame.ethertype {
            ethernet::ARP => {
                if let Some(arp) = Arp::parse(frame.payload) {
                    self.receive_arp(arp, &mut out);
                }
            }
            ethernet::IPV4 => {
                if let Some(packet) = ipv4::parse(frame.payload) {
                    self.receive_ipv4(frame.src, packet, now, &mut out);
                }
            }
            _ => {}
        }
        out
    }

    fn receive_arp(&mut self, arp: Arp, out: &mut Vec<Vec<u8>>) {
        let Some(config) = self.config else { return };
        if arp.sender_ip != Ipv4::UNSPECIFIED {
            self.learn(arp.sender_ip, arp.sender_mac, out);
        }
        if arp.op == arp::REQUEST && arp.target_ip == config.address {
            let reply = Arp {
                op: arp::REPLY,
                sender_mac: self.mac,
                sender_ip: config.address,
                target_mac: arp.sender_mac,
                target_ip: arp.sender_ip,
            };
            out.push(ethernet::build(arp.sender_mac, self.mac, ethernet::ARP, &reply.to_bytes()));
        }
    }

    fn receive_ipv4(&mut self, from: Mac, packet: ipv4::Packet, now: u64, out: &mut Vec<Vec<u8>>) {
        // DHCP answers come before we have an address, to it or broadcast.
        if packet.protocol == ipv4::UDP {
            if let Some(datagram) = udp::parse(packet.payload, packet.src, packet.dst) {
                if datagram.dst_port == dhcp::CLIENT_PORT && datagram.src_port == dhcp::SERVER_PORT {
                    self.receive_dhcp(datagram.payload, now, out);
                }
            }
            return;
        }
        let Some(config) = self.config else { return };
        if packet.dst != config.address || packet.protocol != ipv4::ICMP {
            return;
        }
        let Some(echo) = Echo::parse(packet.payload) else { return };
        match echo.kind {
            icmp::ECHO_REQUEST => {
                // The asker is a neighbour, or behind the router it came through: either way
                // the answer goes back to the MAC it came from.
                if config.address.same_network(packet.src, config.netmask) {
                    self.learn(packet.src, from, out);
                }
                let reply = Echo { kind: icmp::ECHO_REPLY, ..echo }.to_bytes();
                let id = self.take_id();
                let answer = ipv4::build(config.address, packet.src, ipv4::ICMP, id, &reply);
                out.push(ethernet::build(from, self.mac, ethernet::IPV4, &answer));
            }
            icmp::ECHO_REPLY if echo.id == self.ping_id => {
                if let Some(index) = self.pings.iter().position(|&(seq, _)| seq == echo.seq) {
                    let (seq, sent) = self.pings.remove(index);
                    self.events.push(Event::PingReply { from: packet.src, seq, rtt_us: now.saturating_sub(sent) });
                }
            }
            _ => {}
        }
    }

    fn receive_dhcp(&mut self, payload: &[u8], now: u64, out: &mut Vec<Vec<u8>>) {
        let xid = match self.dhcp {
            Dhcp::Discovering { xid, .. } | Dhcp::Requesting { xid, .. } => xid,
            Dhcp::Bound { .. } => return,
        };
        let Some(reply) = dhcp::parse_reply(payload, xid, self.mac) else { return };
        match (&self.dhcp, reply.kind) {
            (Dhcp::Discovering { .. }, dhcp::Kind::Offer) => {
                let Some(server) = reply.server else { return };
                self.dhcp = Dhcp::Requesting { xid, offered: reply.address, server, next_try: now + DHCP_RETRY_US, tries: 1 };
                out.push(self.dhcp_frame(dhcp::request(self.mac, xid, reply.address, server)));
            }
            (&Dhcp::Requesting { server, .. }, dhcp::Kind::Ack) => {
                let lease_secs = reply.lease_secs.unwrap_or(3600);
                let config = Config {
                    address: reply.address,
                    netmask: reply.netmask.unwrap_or(Ipv4([255, 255, 255, 0])),
                    gateway: reply.router,
                    dns: reply.dns,
                    lease_secs,
                };
                let renew_in = (lease_secs as u64 * 1_000_000 / 2).max(MIN_RENEW_US);
                self.dhcp = Dhcp::Bound { server: reply.server.unwrap_or(server), renew_at: now + renew_in };
                if self.config != Some(config) {
                    self.config = Some(config);
                    self.events.push(Event::Configured(config));
                }
            }
            (Dhcp::Requesting { .. }, dhcp::Kind::Nak) => {
                if self.config.take().is_some() {
                    self.events.push(Event::Unconfigured);
                }
                let xid = self.next_xid();
                self.dhcp = Dhcp::Discovering { xid, next_try: now };
            }
            _ => {}
        }
    }

    /// A DHCP message from us, broadcast: we may not have an address yet.
    fn dhcp_frame(&mut self, message: Vec<u8>) -> Vec<u8> {
        let datagram = udp::build(Ipv4::UNSPECIFIED, dhcp::CLIENT_PORT, Ipv4::BROADCAST, dhcp::SERVER_PORT, &message);
        let id = self.take_id();
        let packet = ipv4::build(Ipv4::UNSPECIFIED, Ipv4::BROADCAST, ipv4::UDP, id, &datagram);
        ethernet::build(Mac::BROADCAST, self.mac, ethernet::IPV4, &packet)
    }

    fn take_id(&mut self) -> u16 {
        self.next_id = self.next_id.wrapping_add(1);
        self.next_id
    }

    /// Sends an IPv4 packet: straight to a neighbour, else through the gateway, once ARP says
    /// where.
    fn send_ip(&mut self, dst: Ipv4, protocol: u8, payload: &[u8], now: u64, out: &mut Vec<Vec<u8>>) {
        let Some(config) = self.config else { return };
        let next_hop = if dst.same_network(config.address, config.netmask) {
            dst
        } else {
            let Some(gateway) = config.gateway else { return };
            gateway
        };
        let id = self.take_id();
        let packet = ipv4::build(config.address, dst, protocol, id, payload);
        if let Some(mac) = self.lookup(next_hop) {
            out.push(ethernet::build(mac, self.mac, ethernet::IPV4, &packet));
            return;
        }
        // One packet waits per next hop: a newer one replaces it.
        self.waiting.retain(|waiting| waiting.next_hop != next_hop);
        self.waiting.push(Waiting { next_hop, packet, queued_at: now, asked_at: now });
        if let Some(frame) = self.arp_request(next_hop) {
            out.push(frame);
        }
    }

    fn arp_request(&self, ip: Ipv4) -> Option<Vec<u8>> {
        let config = self.config?;
        let request = Arp { op: arp::REQUEST, sender_mac: self.mac, sender_ip: config.address, target_mac: Mac::default(), target_ip: ip };
        Some(ethernet::build(Mac::BROADCAST, self.mac, ethernet::ARP, &request.to_bytes()))
    }

    fn lookup(&self, ip: Ipv4) -> Option<Mac> {
        self.arp.iter().rev().find(|&&(known, _)| known == ip).map(|&(_, mac)| mac)
    }

    /// Remembers where `ip` is, and sends what was waiting for it.
    fn learn(&mut self, ip: Ipv4, mac: Mac, out: &mut Vec<Vec<u8>>) {
        self.arp.retain(|&(known, _)| known != ip);
        if self.arp.len() == ARP_CACHE {
            self.arp.remove(0);
        }
        self.arp.push((ip, mac));
        let (ready, still): (Vec<_>, Vec<_>) = core::mem::take(&mut self.waiting).into_iter().partition(|w| w.next_hop == ip);
        self.waiting = still;
        for waiting in ready {
            out.push(ethernet::build(mac, self.mac, ethernet::IPV4, &waiting.packet));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::dhcp::tests::{server_reply, CLIENT};

    const ROUTER_MAC: Mac = Mac([0x02, 0x11, 0x22, 0x33, 0x44, 0x55]);
    const ROUTER: Ipv4 = Ipv4([192, 168, 1, 1]);
    const MAC_MAC: Mac = Mac([0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE]);
    const THE_MAC: Ipv4 = Ipv4([192, 168, 1, 20]);
    const OURS: Ipv4 = Ipv4([192, 168, 1, 50]);

    /// The DHCP message inside a frame we sent, with its transaction ID.
    fn dhcp_sent(frame: &[u8]) -> (u8, u32, Vec<u8>) {
        let frame = ethernet::parse(frame).unwrap();
        assert_eq!(frame.dst, Mac::BROADCAST);
        let packet = ipv4::parse(frame.payload).unwrap();
        assert_eq!((packet.src, packet.dst), (Ipv4::UNSPECIFIED, Ipv4::BROADCAST));
        let datagram = udp::parse(packet.payload, packet.src, packet.dst).unwrap();
        assert_eq!((datagram.src_port, datagram.dst_port), (68, 67));
        let message = datagram.payload.to_vec();
        let xid = u32::from_be_bytes(message[4..8].try_into().unwrap());
        (message[242], xid, message)
    }

    /// A frame from the router carrying a DHCP answer.
    fn from_router_dhcp(kind: u8, xid: u32) -> Vec<u8> {
        let message = server_reply(kind, xid, CLIENT, OURS);
        let datagram = udp::build(ROUTER, 67, Ipv4::BROADCAST, 68, &message);
        let packet = ipv4::build(ROUTER, Ipv4::BROADCAST, ipv4::UDP, 1, &datagram);
        ethernet::build(Mac::BROADCAST, ROUTER_MAC, ethernet::IPV4, &packet)
    }

    /// An interface that has gone through DHCP and has `OURS`.
    fn configured() -> Interface {
        let mut interface = Interface::new(CLIENT, 0x1234_5678);
        let (_, xid, _) = dhcp_sent(&interface.poll(0)[0]);
        interface.receive(&from_router_dhcp(2, xid), 10);
        interface.receive(&from_router_dhcp(5, xid), 20);
        interface.take_events();
        interface
    }

    #[test]
    fn dhcp_discovers_requests_and_binds() {
        let mut interface = Interface::new(CLIENT, 0x1234_5678);
        let out = interface.poll(0);
        let (kind, xid, _) = dhcp_sent(&out[0]);
        assert_eq!(kind, 1); // DISCOVER
        // Nothing again until the retry is due.
        assert!(interface.poll(1_000_000).is_empty());
        assert_eq!(dhcp_sent(&interface.poll(DHCP_RETRY_US)[0]).0, 1);

        let out = interface.receive(&from_router_dhcp(2, xid), 5_000_000);
        let (kind, request_xid, message) = dhcp_sent(&out[0]);
        assert_eq!((kind, request_xid), (3, xid)); // REQUEST, same transaction
        assert!(message.windows(6).any(|w| w == [50, 4, 192, 168, 1, 50]));

        assert!(interface.receive(&from_router_dhcp(5, xid), 5_100_000).is_empty());
        let config = interface.config().unwrap();
        assert_eq!((config.address, config.gateway, config.netmask), (OURS, Some(ROUTER), Ipv4([255, 255, 255, 0])));
        assert_eq!(interface.take_events(), [Event::Configured(config)]);
    }

    #[test]
    fn a_lease_is_renewed_halfway_and_lost_on_a_nak() {
        let mut interface = configured();
        // A day's lease: nothing until half of it.
        assert!(interface.poll(40_000_000_000).is_empty());
        let out = interface.poll(43_200_000_020);
        let (kind, xid, _) = dhcp_sent(&out[0]);
        assert_eq!(kind, 3);
        interface.receive(&from_router_dhcp(6, xid), 43_200_000_100);
        assert_eq!(interface.config(), None);
        assert_eq!(interface.take_events(), [Event::Unconfigured]);
        assert_eq!(dhcp_sent(&interface.poll(43_200_000_200)[0]).0, 1); // DISCOVER again
    }

    #[test]
    fn unanswered_requests_start_over_with_a_discover() {
        let mut interface = Interface::new(CLIENT, 99);
        let (_, xid, _) = dhcp_sent(&interface.poll(0)[0]);
        interface.receive(&from_router_dhcp(2, xid), 0);
        let mut now = 0;
        let mut kinds = Vec::new();
        for _ in 0..4 {
            now += DHCP_RETRY_US;
            kinds.extend(interface.poll(now).iter().map(|frame| dhcp_sent(frame).0));
        }
        assert_eq!(kinds, [3, 3, 1, 1]);
    }

    #[test]
    fn arp_requests_for_our_address_are_answered() {
        let mut interface = configured();
        let request = Arp { op: arp::REQUEST, sender_mac: MAC_MAC, sender_ip: THE_MAC, target_mac: Mac::default(), target_ip: OURS };
        let out = interface.receive(&ethernet::build(Mac::BROADCAST, MAC_MAC, ethernet::ARP, &request.to_bytes()), 0);
        let frame = ethernet::parse(&out[0]).unwrap();
        assert_eq!((frame.dst, frame.ethertype), (MAC_MAC, ethernet::ARP));
        let reply = Arp::parse(frame.payload).unwrap();
        assert_eq!((reply.op, reply.sender_mac, reply.sender_ip, reply.target_ip), (arp::REPLY, CLIENT, OURS, THE_MAC));
        // For someone else's address, silence.
        let other = Arp { target_ip: Ipv4([192, 168, 1, 99]), ..request };
        assert!(interface.receive(&ethernet::build(Mac::BROADCAST, MAC_MAC, ethernet::ARP, &other.to_bytes()), 0).is_empty());
    }

    #[test]
    fn pings_to_us_are_answered() {
        let mut interface = configured();
        let echo = Echo { kind: icmp::ECHO_REQUEST, id: 7, seq: 3, data: b"hello" }.to_bytes();
        let packet = ipv4::build(THE_MAC, OURS, ipv4::ICMP, 1, &echo);
        let out = interface.receive(&ethernet::build(CLIENT, MAC_MAC, ethernet::IPV4, &packet), 0);
        let frame = ethernet::parse(&out[0]).unwrap();
        assert_eq!(frame.dst, MAC_MAC);
        let reply = ipv4::parse(frame.payload).unwrap();
        assert_eq!((reply.src, reply.dst), (OURS, THE_MAC));
        assert_eq!(Echo::parse(reply.payload), Some(Echo { kind: icmp::ECHO_REPLY, id: 7, seq: 3, data: b"hello" }));
        // Frames for another MAC are none of our business.
        assert!(interface.receive(&ethernet::build(MAC_MAC, ROUTER_MAC, ethernet::IPV4, &packet), 0).is_empty());
    }

    #[test]
    fn our_pings_resolve_the_neighbour_first_and_report_the_reply() {
        let mut interface = configured();
        let out = interface.ping(THE_MAC, 1, 1_000);
        let arp = Arp::parse(ethernet::parse(&out[0]).unwrap().payload).unwrap();
        assert_eq!((arp.op, arp.target_ip), (arp::REQUEST, THE_MAC));

        // The answer lets the waiting ping go.
        let answer = Arp { op: arp::REPLY, sender_mac: MAC_MAC, sender_ip: THE_MAC, target_mac: CLIENT, target_ip: OURS };
        let out = interface.receive(&ethernet::build(CLIENT, MAC_MAC, ethernet::ARP, &answer.to_bytes()), 2_000);
        let frame = ethernet::parse(&out[0]).unwrap();
        assert_eq!(frame.dst, MAC_MAC);
        let packet = ipv4::parse(frame.payload).unwrap();
        let echo = Echo::parse(packet.payload).unwrap();
        assert_eq!((echo.kind, echo.seq), (icmp::ECHO_REQUEST, 1));

        // The reply comes back: an event with the round trip.
        let reply = Echo { kind: icmp::ECHO_REPLY, ..echo }.to_bytes();
        let reply = ipv4::build(THE_MAC, OURS, ipv4::ICMP, 9, &reply);
        interface.receive(&ethernet::build(CLIENT, MAC_MAC, ethernet::IPV4, &reply), 5_000);
        assert_eq!(interface.take_events(), [Event::PingReply { from: THE_MAC, seq: 1, rtt_us: 4_000 }]);
        // A second ping goes straight out: the neighbour is known now.
        let out = interface.ping(THE_MAC, 2, 6_000);
        assert_eq!(ethernet::parse(&out[0]).unwrap().ethertype, ethernet::IPV4);
    }

    #[test]
    fn far_away_hosts_are_reached_through_the_gateway() {
        let mut interface = configured();
        let out = interface.ping(Ipv4([1, 1, 1, 1]), 1, 0);
        let arp = Arp::parse(ethernet::parse(&out[0]).unwrap().payload).unwrap();
        assert_eq!(arp.target_ip, ROUTER);
    }

    #[test]
    fn unanswered_arp_is_asked_again_then_given_up() {
        let mut interface = configured();
        interface.ping(THE_MAC, 1, 0);
        assert!(interface.poll(500_000).is_empty());
        assert_eq!(interface.poll(ARP_RETRY_US).len(), 1);
        assert!(interface.poll(ARP_GIVE_UP_US).is_empty());
        // Given up: an answer now finds nothing waiting.
        let answer = Arp { op: arp::REPLY, sender_mac: MAC_MAC, sender_ip: THE_MAC, target_mac: CLIENT, target_ip: OURS };
        assert!(interface.receive(&ethernet::build(CLIENT, MAC_MAC, ethernet::ARP, &answer.to_bytes()), ARP_GIVE_UP_US + 1).is_empty());
    }

    #[test]
    fn nothing_is_sent_or_answered_without_an_address() {
        let mut interface = Interface::new(CLIENT, 5);
        assert!(interface.ping(THE_MAC, 1, 0).is_empty());
        let echo = Echo { kind: icmp::ECHO_REQUEST, id: 7, seq: 3, data: b"" }.to_bytes();
        let packet = ipv4::build(THE_MAC, OURS, ipv4::ICMP, 1, &echo);
        assert!(interface.receive(&ethernet::build(CLIENT, MAC_MAC, ethernet::IPV4, &packet), 0).is_empty());
    }
}
