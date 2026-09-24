// net/dhcp.rs
//! DHCP, the client's side (RFC 2131): asking for an address (DISCOVER), taking one offered
//! (REQUEST), and reading the server's OFFER, ACK or NAK.

use alloc::vec::Vec;
use super::{ipv4_at, u16_at, Ipv4, Mac};

pub const SERVER_PORT: u16 = 67;
pub const CLIENT_PORT: u16 = 68;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Discover = 1,
    Offer = 2,
    Request = 3,
    Ack = 5,
    Nak = 6,
}

/// Fixed BOOTP fields, then the magic cookie that says DHCP options follow.
const FIXED: usize = 236;
const MAGIC: [u8; 4] = [99, 130, 83, 99];
/// Some servers ignore anything shorter (BOOTP's size).
const MIN_LENGTH: usize = 300;

// Options.
const PAD: u8 = 0;
const SUBNET_MASK: u8 = 1;
const ROUTER: u8 = 3;
const DNS: u8 = 6;
const REQUESTED_IP: u8 = 50;
const LEASE_TIME: u8 = 51;
const MESSAGE_TYPE: u8 = 53;
const SERVER_ID: u8 = 54;
const PARAMETERS: u8 = 55;
const END: u8 = 255;

/// A message from the client, broadcast: `kind` with its options.
fn message(mac: Mac, xid: u32, kind: Kind, options: &[(u8, &[u8])]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(MIN_LENGTH);
    // BOOTREQUEST, Ethernet, 6-byte addresses, no hops.
    bytes.extend_from_slice(&[1, 1, 6, 0]);
    bytes.extend_from_slice(&xid.to_be_bytes());
    // Seconds elapsed, then flags: broadcast the answer, since we can't take unicast yet.
    bytes.extend_from_slice(&[0, 0, 0x80, 0]);
    // ciaddr, yiaddr, siaddr, giaddr.
    bytes.extend_from_slice(&[0; 16]);
    bytes.extend_from_slice(&mac.0);
    bytes.resize(FIXED, 0); // rest of chaddr, sname, file
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&[MESSAGE_TYPE, 1, kind as u8]);
    for (code, value) in options {
        bytes.push(*code);
        bytes.push(value.len() as u8);
        bytes.extend_from_slice(value);
    }
    bytes.extend_from_slice(&[PARAMETERS, 3, SUBNET_MASK, ROUTER, DNS, END]);
    bytes.resize(bytes.len().max(MIN_LENGTH), PAD);
    bytes
}

/// Asks any server for an address.
pub fn discover(mac: Mac, xid: u32) -> Vec<u8> {
    message(mac, xid, Kind::Discover, &[])
}

/// Takes `address`, offered by `server` (or asks it to renew the lease on it).
pub fn request(mac: Mac, xid: u32, address: Ipv4, server: Ipv4) -> Vec<u8> {
    message(mac, xid, Kind::Request, &[(REQUESTED_IP, &address.0), (SERVER_ID, &server.0)])
}

/// A server's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reply {
    pub kind: Kind,
    /// The address offered or given.
    pub address: Ipv4,
    pub server: Option<Ipv4>,
    pub netmask: Option<Ipv4>,
    pub router: Option<Ipv4>,
    pub dns: Option<Ipv4>,
    pub lease_secs: Option<u32>,
}

/// A server's answer to our transaction `xid`, for `mac`. Anything else (another client's,
/// or not an answer) is `None`.
pub fn parse_reply(bytes: &[u8], xid: u32, mac: Mac) -> Option<Reply> {
    if bytes.len() < FIXED + 4 || bytes[0] != 2 || bytes[4..8] != xid.to_be_bytes() || bytes[28..34] != mac.0 {
        return None;
    }
    if bytes[FIXED..FIXED + 4] != MAGIC {
        return None;
    }
    let mut reply = Reply {
        kind: Kind::Offer,
        address: ipv4_at(bytes, 16),
        server: None,
        netmask: None,
        router: None,
        dns: None,
        lease_secs: None,
    };
    let mut kind = None;
    let mut at = FIXED + 4;
    while at < bytes.len() {
        let code = bytes[at];
        match code {
            PAD => {
                at += 1;
                continue;
            }
            END => break,
            _ => {}
        }
        let length = *bytes.get(at + 1)? as usize;
        let value = bytes.get(at + 2..at + 2 + length)?;
        let address = (length >= 4).then(|| ipv4_at(value, 0));
        match code {
            MESSAGE_TYPE if length == 1 => {
                kind = match value[0] {
                    2 => Some(Kind::Offer),
                    5 => Some(Kind::Ack),
                    6 => Some(Kind::Nak),
                    _ => None,
                }
            }
            SUBNET_MASK => reply.netmask = address,
            ROUTER => reply.router = address,
            DNS => reply.dns = address,
            SERVER_ID => reply.server = address,
            LEASE_TIME if length == 4 => reply.lease_secs = Some((u16_at(value, 0) as u32) << 16 | u16_at(value, 2) as u32),
            _ => {}
        }
        at += 2 + length;
    }
    reply.kind = kind?;
    Some(reply)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const CLIENT: Mac = Mac([0xB8, 0x27, 0xEB, 1, 2, 3]);

    /// A server's answer, as a router would send it.
    pub fn server_reply(kind: u8, xid: u32, mac: Mac, address: Ipv4) -> Vec<u8> {
        let mut bytes = vec![2, 1, 6, 0];
        bytes.extend_from_slice(&xid.to_be_bytes());
        bytes.extend_from_slice(&[0; 8]);
        bytes.extend_from_slice(&address.0);
        bytes.extend_from_slice(&[192, 168, 1, 1, 0, 0, 0, 0]);
        bytes.extend_from_slice(&mac.0);
        bytes.resize(FIXED, 0);
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&[MESSAGE_TYPE, 1, kind, PAD]);
        bytes.extend_from_slice(&[SERVER_ID, 4, 192, 168, 1, 1]);
        bytes.extend_from_slice(&[SUBNET_MASK, 4, 255, 255, 255, 0]);
        bytes.extend_from_slice(&[ROUTER, 4, 192, 168, 1, 1]);
        bytes.extend_from_slice(&[DNS, 8, 1, 1, 1, 1, 8, 8, 8, 8]);
        bytes.extend_from_slice(&[LEASE_TIME, 4, 0, 1, 0x51, 0x80]); // a day
        bytes.push(END);
        bytes
    }

    #[test]
    fn client_messages_carry_their_options() {
        let discover = discover(CLIENT, 0xDEADBEEF);
        assert!(discover.len() >= MIN_LENGTH);
        assert_eq!(&discover[4..8], &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(&discover[28..34], &CLIENT.0);
        assert_eq!(&discover[FIXED..FIXED + 7], &[99, 130, 83, 99, MESSAGE_TYPE, 1, 1]);
        let request = request(CLIENT, 1, Ipv4([192, 168, 1, 50]), Ipv4([192, 168, 1, 1]));
        let options = &request[FIXED + 7..];
        assert_eq!(&options[..12], &[REQUESTED_IP, 4, 192, 168, 1, 50, SERVER_ID, 4, 192, 168, 1, 1]);
    }

    #[test]
    fn server_replies_parse() {
        let bytes = server_reply(5, 7, CLIENT, Ipv4([192, 168, 1, 50]));
        let reply = parse_reply(&bytes, 7, CLIENT).unwrap();
        assert_eq!(reply.kind, Kind::Ack);
        assert_eq!(reply.address, Ipv4([192, 168, 1, 50]));
        assert_eq!(reply.server, Some(Ipv4([192, 168, 1, 1])));
        assert_eq!(reply.netmask, Some(Ipv4([255, 255, 255, 0])));
        assert_eq!(reply.dns, Some(Ipv4([1, 1, 1, 1])));
        assert_eq!(reply.lease_secs, Some(86_400));
    }

    #[test]
    fn replies_to_others_or_broken_ones_are_ignored() {
        let bytes = server_reply(2, 7, CLIENT, Ipv4([192, 168, 1, 50]));
        assert!(parse_reply(&bytes, 8, CLIENT).is_none()); // another transaction
        assert!(parse_reply(&bytes, 7, Mac([2; 6])).is_none()); // another client
        assert!(parse_reply(&bytes[..FIXED], 7, CLIENT).is_none());
        let mut cut = bytes.clone();
        cut.truncate(bytes.len() - 6); // an option runs off the end
        assert!(parse_reply(&cut, 7, CLIENT).is_none());
        let untyped = discover(CLIENT, 7); // a request, not a reply
        assert!(parse_reply(&untyped, 7, CLIENT).is_none());
    }
}
