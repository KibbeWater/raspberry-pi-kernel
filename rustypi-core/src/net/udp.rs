// net/udp.rs
//! UDP datagrams, with the checksum over the IPv4 pseudo-header.

use alloc::vec::Vec;
use super::{finish, sum, u16_at, Ipv4};

const HEADER: usize = 8;

#[derive(Debug, PartialEq, Eq)]
pub struct Datagram<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub payload: &'a [u8],
}

/// The checksum of `datagram` (with its own checksum field as it is) between `src` and `dst`.
fn datagram_checksum(src: Ipv4, dst: Ipv4, datagram: &[u8]) -> u16 {
    let mut acc = sum(0, &src.0);
    acc = sum(acc, &dst.0);
    acc += super::ipv4::UDP as u32 + datagram.len() as u32;
    finish(sum(acc, datagram))
}

/// A datagram that arrived from `src` for `dst`. A checksum of 0 means none was sent.
pub fn parse(bytes: &[u8], src: Ipv4, dst: Ipv4) -> Option<Datagram<'_>> {
    if bytes.len() < HEADER {
        return None;
    }
    let length = u16_at(bytes, 4) as usize;
    if length < HEADER || length > bytes.len() {
        return None;
    }
    let datagram = &bytes[..length];
    if u16_at(bytes, 6) != 0 && datagram_checksum(src, dst, datagram) != 0 {
        return None;
    }
    Some(Datagram { src_port: u16_at(bytes, 0), dst_port: u16_at(bytes, 2), payload: &datagram[HEADER..] })
}

pub fn build(src: Ipv4, src_port: u16, dst: Ipv4, dst_port: u16, payload: &[u8]) -> Vec<u8> {
    let length = HEADER + payload.len();
    let mut datagram = Vec::with_capacity(length);
    datagram.extend_from_slice(&src_port.to_be_bytes());
    datagram.extend_from_slice(&dst_port.to_be_bytes());
    datagram.extend_from_slice(&(length as u16).to_be_bytes());
    datagram.extend_from_slice(&[0, 0]);
    datagram.extend_from_slice(payload);
    // A computed 0 is sent as all ones: 0 means "no checksum".
    let checksum = match datagram_checksum(src, dst, &datagram) {
        0 => 0xFFFF,
        checksum => checksum,
    };
    datagram[6..8].copy_from_slice(&checksum.to_be_bytes());
    datagram
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Ipv4 = Ipv4([192, 168, 1, 2]);
    const B: Ipv4 = Ipv4([192, 168, 1, 3]);

    #[test]
    fn datagrams_round_trip_and_bad_checksums_are_refused() {
        let datagram = build(A, 68, B, 67, b"payload");
        assert_eq!(parse(&datagram, A, B), Some(Datagram { src_port: 68, dst_port: 67, payload: b"payload" }));
        // The pseudo-header counts: the same bytes between other addresses don't check out.
        assert_eq!(parse(&datagram, A, Ipv4([192, 168, 1, 4])), None);
        let mut unchecked = datagram.clone();
        unchecked[6..8].fill(0);
        unchecked[8] ^= 1;
        assert!(parse(&unchecked, A, B).is_some());
        assert_eq!(parse(&datagram[..7], A, B), None);
    }
}
