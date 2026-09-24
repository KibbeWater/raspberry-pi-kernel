// net/icmp.rs
//! ICMP echo, which is ping.

use alloc::vec::Vec;
use super::{checksum, u16_at};

pub const ECHO_REPLY: u8 = 0;
pub const ECHO_REQUEST: u8 = 8;

#[derive(Debug, PartialEq, Eq)]
pub struct Echo<'a> {
    /// `ECHO_REQUEST` or `ECHO_REPLY`.
    pub kind: u8,
    pub id: u16,
    pub seq: u16,
    pub data: &'a [u8],
}

impl<'a> Echo<'a> {
    pub fn parse(bytes: &'a [u8]) -> Option<Echo<'a>> {
        if bytes.len() < 8 || !matches!(bytes[0], ECHO_REQUEST | ECHO_REPLY) || bytes[1] != 0 || checksum(bytes) != 0 {
            return None;
        }
        Some(Echo { kind: bytes[0], id: u16_at(bytes, 4), seq: u16_at(bytes, 6), data: &bytes[8..] })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 + self.data.len());
        bytes.extend_from_slice(&[self.kind, 0, 0, 0]);
        bytes.extend_from_slice(&self.id.to_be_bytes());
        bytes.extend_from_slice(&self.seq.to_be_bytes());
        bytes.extend_from_slice(self.data);
        let sum = checksum(&bytes).to_be_bytes();
        bytes[2..4].copy_from_slice(&sum);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echoes_round_trip_with_their_checksum() {
        let echo = Echo { kind: ECHO_REQUEST, id: 0x1234, seq: 7, data: b"abc" };
        let bytes = echo.to_bytes();
        assert_eq!(Echo::parse(&bytes), Some(echo));
        let mut corrupt = bytes.clone();
        corrupt[9] ^= 0xFF;
        assert_eq!(Echo::parse(&corrupt), None);
        let mut unreachable = bytes;
        unreachable[0] = 3;
        assert_eq!(Echo::parse(&unreachable), None);
    }
}
