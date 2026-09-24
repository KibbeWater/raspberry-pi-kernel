// net/ipv4.rs
//! IPv4 packets. Fragments are refused: nothing here sends anything that big.

use alloc::vec::Vec;
use super::{checksum, ipv4_at, u16_at, Ipv4};

pub const ICMP: u8 = 1;
pub const UDP: u8 = 17;

const HEADER: usize = 20;
const TTL: u8 = 64;
/// Flags: don't fragment. And the more-fragments flag and offset, which say it is one.
const DONT_FRAGMENT: u16 = 0x4000;
const FRAGMENTED: u16 = 0x3FFF;

#[derive(Debug, PartialEq, Eq)]
pub struct Packet<'a> {
    pub src: Ipv4,
    pub dst: Ipv4,
    pub protocol: u8,
    pub payload: &'a [u8],
}

pub fn parse(bytes: &[u8]) -> Option<Packet<'_>> {
    if bytes.len() < HEADER || bytes[0] >> 4 != 4 {
        return None;
    }
    let header = (bytes[0] & 0x0F) as usize * 4;
    let total = u16_at(bytes, 2) as usize;
    if header < HEADER || total < header || total > bytes.len() || checksum(&bytes[..header]) != 0 {
        return None;
    }
    if u16_at(bytes, 6) & FRAGMENTED != 0 {
        return None;
    }
    Some(Packet { src: ipv4_at(bytes, 12), dst: ipv4_at(bytes, 16), protocol: bytes[9], payload: &bytes[header..total] })
}

/// A packet from `src` to `dst` carrying `payload`, with identification `id`.
pub fn build(src: Ipv4, dst: Ipv4, protocol: u8, id: u16, payload: &[u8]) -> Vec<u8> {
    let total = HEADER + payload.len();
    let mut packet = Vec::with_capacity(total);
    packet.extend_from_slice(&[0x45, 0]);
    packet.extend_from_slice(&(total as u16).to_be_bytes());
    packet.extend_from_slice(&id.to_be_bytes());
    packet.extend_from_slice(&DONT_FRAGMENT.to_be_bytes());
    packet.extend_from_slice(&[TTL, protocol, 0, 0]);
    packet.extend_from_slice(&src.0);
    packet.extend_from_slice(&dst.0);
    let sum = checksum(&packet).to_be_bytes();
    packet[10..12].copy_from_slice(&sum);
    packet.extend_from_slice(payload);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Ipv4 = Ipv4([10, 0, 0, 1]);
    const B: Ipv4 = Ipv4([10, 0, 0, 2]);

    #[test]
    fn packets_round_trip_and_ignore_trailing_padding() {
        let mut packet = build(A, B, UDP, 7, b"hi");
        packet.extend_from_slice(&[0; 10]); // Ethernet padding
        let parsed = parse(&packet).unwrap();
        assert_eq!((parsed.src, parsed.dst, parsed.protocol, parsed.payload), (A, B, UDP, &b"hi"[..]));
    }

    #[test]
    fn broken_or_fragmented_packets_are_refused() {
        let packet = build(A, B, ICMP, 1, b"data");
        let mut corrupt = packet.clone();
        corrupt[15] ^= 1;
        assert_eq!(parse(&corrupt), None);
        assert_eq!(parse(&packet[..22]), None); // shorter than it says
        let mut fragment = packet.clone();
        fragment[6] = 0x20; // more fragments
        fragment[10..12].fill(0);
        let sum = checksum(&fragment[..20]).to_be_bytes();
        fragment[10..12].copy_from_slice(&sum);
        assert_eq!(parse(&fragment), None);
        let mut ipv6 = packet;
        ipv6[0] = 0x60;
        assert_eq!(parse(&ipv6), None);
    }
}
