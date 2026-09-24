// net/ethernet.rs
//! Ethernet II frames, without the FCS (the controller adds and checks it).

use alloc::vec::Vec;
use super::{mac_at, u16_at, Mac};

pub const ARP: u16 = 0x0806;
pub const IPV4: u16 = 0x0800;

pub const HEADER: usize = 14;
/// Shorter frames are padded to this (60 bytes, plus the controller's 4-byte FCS).
const MIN_FRAME: usize = 60;

#[derive(Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub dst: Mac,
    pub src: Mac,
    pub ethertype: u16,
    /// With any padding still on: the protocol inside knows its own length.
    pub payload: &'a [u8],
}

pub fn parse(bytes: &[u8]) -> Option<Frame<'_>> {
    if bytes.len() < HEADER {
        return None;
    }
    Some(Frame { dst: mac_at(bytes, 0), src: mac_at(bytes, 6), ethertype: u16_at(bytes, 12), payload: &bytes[HEADER..] })
}

pub fn build(dst: Mac, src: Mac, ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity((HEADER + payload.len()).max(MIN_FRAME));
    frame.extend_from_slice(&dst.0);
    frame.extend_from_slice(&src.0);
    frame.extend_from_slice(&ethertype.to_be_bytes());
    frame.extend_from_slice(payload);
    frame.resize(frame.len().max(MIN_FRAME), 0);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_short_ones_are_padded() {
        let frame = build(Mac::BROADCAST, Mac([2, 0, 0, 0, 0, 1]), ARP, &[1, 2, 3]);
        assert_eq!(frame.len(), 60);
        let parsed = parse(&frame).unwrap();
        assert_eq!((parsed.dst, parsed.src, parsed.ethertype), (Mac::BROADCAST, Mac([2, 0, 0, 0, 0, 1]), ARP));
        assert_eq!(&parsed.payload[..3], &[1, 2, 3]);
        assert_eq!(parse(&frame[..13]), None);
    }
}
