// net/arp.rs
//! ARP for IPv4 over Ethernet: who has an IP address, and the answer.

use super::{ipv4_at, mac_at, u16_at, Ipv4, Mac};

pub const REQUEST: u16 = 1;
pub const REPLY: u16 = 2;

const LENGTH: usize = 28;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arp {
    pub op: u16,
    pub sender_mac: Mac,
    pub sender_ip: Ipv4,
    pub target_mac: Mac,
    pub target_ip: Ipv4,
}

impl Arp {
    pub fn parse(bytes: &[u8]) -> Option<Arp> {
        // Ethernet hardware, IPv4 protocol, their address lengths.
        if bytes.len() < LENGTH || u16_at(bytes, 0) != 1 || u16_at(bytes, 2) != 0x0800 || bytes[4] != 6 || bytes[5] != 4 {
            return None;
        }
        Some(Arp {
            op: u16_at(bytes, 6),
            sender_mac: mac_at(bytes, 8),
            sender_ip: ipv4_at(bytes, 14),
            target_mac: mac_at(bytes, 18),
            target_ip: ipv4_at(bytes, 24),
        })
    }

    pub fn to_bytes(&self) -> [u8; LENGTH] {
        let mut bytes = [0; LENGTH];
        bytes[..6].copy_from_slice(&[0, 1, 0x08, 0x00, 6, 4]);
        bytes[6..8].copy_from_slice(&self.op.to_be_bytes());
        bytes[8..14].copy_from_slice(&self.sender_mac.0);
        bytes[14..18].copy_from_slice(&self.sender_ip.0);
        bytes[18..24].copy_from_slice(&self.target_mac.0);
        bytes[24..28].copy_from_slice(&self.target_ip.0);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arp_round_trips_and_other_kinds_are_refused() {
        let arp = Arp {
            op: REQUEST,
            sender_mac: Mac([2, 0, 0, 0, 0, 1]),
            sender_ip: Ipv4([10, 0, 0, 1]),
            target_mac: Mac::default(),
            target_ip: Ipv4([10, 0, 0, 2]),
        };
        let bytes = arp.to_bytes();
        assert_eq!(Arp::parse(&bytes), Some(arp));
        assert_eq!(Arp::parse(&bytes[..27]), None);
        let mut ipv6 = bytes;
        ipv6[2..4].copy_from_slice(&0x86DDu16.to_be_bytes());
        assert_eq!(Arp::parse(&ipv6), None);
    }
}
