// net/mod.rs
//! A small IPv4 network stack: frames in, frames out. Each protocol is parsing and building
//! (`ethernet`, `arp`, `ipv4`, `icmp`, `udp`, `dhcp`), and `interface` ties them together:
//! it answers ARP and pings, pings others, and gets an address by DHCP. The kernel moves the
//! frames, through `lan7800`'s framing, and supplies the time.

pub mod arp;
pub mod dhcp;
pub mod dns;
pub mod ethernet;
pub mod icmp;
pub mod interface;
pub mod ipv4;
pub mod lan7800;
pub mod sntp;
pub mod udp;

use core::fmt;

/// An Ethernet (MAC) address.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mac(pub [u8; 6]);

impl Mac {
    pub const BROADCAST: Mac = Mac([0xFF; 6]);
}

impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

/// An IPv4 address.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ipv4(pub [u8; 4]);

impl Ipv4 {
    pub const UNSPECIFIED: Ipv4 = Ipv4([0; 4]);
    pub const BROADCAST: Ipv4 = Ipv4([255; 4]);

    /// Dotted decimal, like `192.168.1.10`.
    pub fn parse(text: &str) -> Option<Ipv4> {
        let mut octets = [0; 4];
        let mut parts = text.split('.');
        for octet in &mut octets {
            let part = parts.next()?;
            if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            *octet = part.parse().ok()?;
        }
        parts.next().is_none().then_some(Ipv4(octets))
    }

    fn bits(self) -> u32 {
        u32::from_be_bytes(self.0)
    }

    /// Whether `self` and `other` are on the same network under `netmask`.
    pub fn same_network(self, other: Ipv4, netmask: Ipv4) -> bool {
        self.bits() & netmask.bits() == other.bits() & netmask.bits()
    }
}

impl fmt::Display for Ipv4 {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let [a, b, c, d] = self.0;
        write!(f, "{a}.{b}.{c}.{d}")
    }
}

/// Adds `data` to a running one's complement sum, as 16-bit big endian words.
fn sum(mut acc: u32, data: &[u8]) -> u32 {
    let mut words = data.chunks_exact(2);
    for word in &mut words {
        acc += u16::from_be_bytes([word[0], word[1]]) as u32;
    }
    if let [last] = words.remainder() {
        acc += (*last as u32) << 8;
    }
    acc
}

/// Folds a running sum into the Internet checksum (RFC 1071).
fn finish(mut acc: u32) -> u16 {
    while acc >> 16 != 0 {
        acc = (acc & 0xFFFF) + (acc >> 16);
    }
    !(acc as u16)
}

/// The Internet checksum of `data`. Over data that includes its own correct checksum, it is 0.
pub fn checksum(data: &[u8]) -> u16 {
    finish(sum(0, data))
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

fn ipv4_at(bytes: &[u8], at: usize) -> Ipv4 {
    Ipv4([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn mac_at(bytes: &[u8], at: usize) -> Mac {
    let mut mac = [0; 6];
    mac.copy_from_slice(&bytes[at..at + 6]);
    Mac(mac)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    #[test]
    fn addresses_parse_and_print() {
        assert_eq!(Ipv4::parse("192.168.1.10"), Some(Ipv4([192, 168, 1, 10])));
        for bad in ["", "1.2.3", "1.2.3.4.5", "256.1.1.1", "1..2.3", "a.b.c.d", "1.2.3.-4", "01234.1.1.1"] {
            assert_eq!(Ipv4::parse(bad), None, "{bad}");
        }
        assert_eq!(format!("{}", Ipv4([10, 0, 0, 1])), "10.0.0.1");
        assert_eq!(format!("{}", Mac([0xB8, 0x27, 0xEB, 1, 2, 0xAB])), "b8:27:eb:01:02:ab");
    }

    #[test]
    fn networks_compare_under_the_netmask() {
        let mask = Ipv4([255, 255, 255, 0]);
        assert!(Ipv4([192, 168, 1, 10]).same_network(Ipv4([192, 168, 1, 200]), mask));
        assert!(!Ipv4([192, 168, 1, 10]).same_network(Ipv4([192, 168, 2, 10]), mask));
    }

    #[test]
    fn checksums_match_rfc_1071() {
        // RFC 1071's example words sum to 0xDDF2, so the checksum is its complement.
        assert_eq!(checksum(&[0x00, 0x01, 0xF2, 0x03, 0xF4, 0xF5, 0xF6, 0xF7]), !0xDDF2);
        // An odd length pads with a zero byte.
        assert_eq!(checksum(&[0xAB]), !0xAB00);
        // Data including its correct checksum sums to 0.
        let mut header = [0x45, 0x00, 0x00, 0x1C, 0, 0, 0x40, 0, 64, 1, 0, 0, 10, 0, 0, 1, 10, 0, 0, 2];
        let sum = checksum(&header).to_be_bytes();
        header[10..12].copy_from_slice(&sum);
        assert_eq!(checksum(&header), 0);
    }
}
