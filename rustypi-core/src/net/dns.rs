// net/dns.rs
//! DNS (RFC 1035), as far as asking a server for a name's IPv4 addresses (A records).

use alloc::vec::Vec;
use super::{ipv4_at, u16_at, Ipv4};

pub const PORT: u16 = 53;
const HEADER: usize = 12;
/// Recursion desired: the server looks the name up for us.
const RECURSION_DESIRED: u16 = 1 << 8;
const RESPONSE: u16 = 1 << 15;
const TYPE_A: u16 = 1;
const CLASS_IN: u16 = 1;
/// Longest name DNS allows, and longest label.
const MAX_NAME: usize = 253;
const MAX_LABEL: usize = 63;

/// A query for `name`'s A records, with ID `id`. `None` for a name DNS can't carry.
pub fn query(name: &str, id: u16) -> Option<Vec<u8>> {
    let name = name.trim_end_matches('.');
    if name.is_empty() || name.len() > MAX_NAME {
        return None;
    }
    let mut bytes = Vec::with_capacity(HEADER + name.len() + 6);
    bytes.extend_from_slice(&id.to_be_bytes());
    bytes.extend_from_slice(&RECURSION_DESIRED.to_be_bytes());
    bytes.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // one question
    for label in name.split('.') {
        if label.is_empty() || label.len() > MAX_LABEL || !label.is_ascii() {
            return None;
        }
        bytes.push(label.len() as u8);
        bytes.extend_from_slice(label.as_bytes());
    }
    bytes.push(0);
    bytes.extend_from_slice(&TYPE_A.to_be_bytes());
    bytes.extend_from_slice(&CLASS_IN.to_be_bytes());
    Some(bytes)
}

/// What a server said about our query.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    /// The name's addresses (empty if it has none, just other records).
    Addresses(Vec<Ipv4>),
    /// No such name.
    NoSuchName,
    /// The server couldn't answer (its response code).
    Failed(u8),
}

/// Where the name at `at` ends: past its labels, or past the pointer that ends it (pointers
/// are only skipped, never followed, so they can't loop).
fn skip_name(bytes: &[u8], mut at: usize) -> Option<usize> {
    for _ in 0..MAX_NAME {
        let length = *bytes.get(at)?;
        match length {
            0 => return Some(at + 1),
            _ if length & 0xC0 == 0xC0 => return (at + 2 <= bytes.len()).then_some(at + 2),
            _ if length & 0xC0 != 0 => return None,
            _ => at += 1 + length as usize,
        }
    }
    None
}

/// The answer to our query `id`, or `None` if `bytes` isn't one (another query's, or broken).
pub fn parse(bytes: &[u8], id: u16) -> Option<Answer> {
    if bytes.len() < HEADER || u16_at(bytes, 0) != id || u16_at(bytes, 2) & RESPONSE == 0 {
        return None;
    }
    match (u16_at(bytes, 2) & 0xF) as u8 {
        0 => {}
        3 => return Some(Answer::NoSuchName),
        code => return Some(Answer::Failed(code)),
    }
    let questions = u16_at(bytes, 4);
    let answers = u16_at(bytes, 6);
    let mut at = HEADER;
    for _ in 0..questions {
        at = skip_name(bytes, at)? + 4;
    }
    let mut addresses = Vec::new();
    for _ in 0..answers {
        at = skip_name(bytes, at)?;
        let record = bytes.get(at..at + 10)?;
        let (kind, class, length) = (u16_at(record, 0), u16_at(record, 2), u16_at(record, 8) as usize);
        let data = bytes.get(at + 10..at + 10 + length)?;
        if kind == TYPE_A && class == CLASS_IN && length == 4 {
            addresses.push(ipv4_at(data, 0));
        }
        at += 10 + length;
    }
    Some(Answer::Addresses(addresses))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server's answer to `query`: a CNAME to another name, then that name's two A records,
    /// with compression pointers like servers send.
    fn answer(query: &[u8]) -> Vec<u8> {
        let mut bytes = query[..2].to_vec();
        bytes.extend_from_slice(&[0x81, 0x80, 0, 1, 0, 3, 0, 0, 0, 0]);
        bytes.extend_from_slice(&query[HEADER..]); // the question
        // CNAME: name -> pointer to the question's name (offset 12).
        bytes.extend_from_slice(&[0xC0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 6, 3, b'w', b'w', b'w', 0xC0, 12]);
        let cname = bytes.len() - 6;
        for address in [[93, 184, 216, 34], [93, 184, 216, 35]] {
            bytes.extend_from_slice(&[0xC0, cname as u8, 0, 1, 0, 1, 0, 0, 1, 0, 0, 4]);
            bytes.extend_from_slice(&address);
        }
        bytes
    }

    #[test]
    fn queries_spell_the_name_in_labels() {
        let asked = query("example.com", 0xBEEF).unwrap();
        assert_eq!(&asked[..4], &[0xBE, 0xEF, 0x01, 0x00]);
        assert_eq!(&asked[HEADER..], b"\x07example\x03com\x00\x00\x01\x00\x01");
        assert_eq!(query("example.com.", 1).unwrap()[HEADER..], asked[HEADER..]);
        for bad in ["", "a..b", ".com", &"x".repeat(64), &"a.".repeat(130), "é.com"] {
            assert_eq!(query(bad, 1), None, "{bad}");
        }
    }

    #[test]
    fn answers_give_the_addresses_through_cnames_and_pointers() {
        let query = query("example.com", 7).unwrap();
        let answer = answer(&query);
        assert_eq!(parse(&answer, 7), Some(Answer::Addresses(vec![Ipv4([93, 184, 216, 34]), Ipv4([93, 184, 216, 35])])));
        assert_eq!(parse(&answer, 8), None); // another query's
        assert_eq!(parse(&query, 7), None); // a query, not an answer
    }

    #[test]
    fn failures_and_broken_answers() {
        let query = query("nope.invalid", 3).unwrap();
        let mut missing = query.clone();
        missing[2..4].copy_from_slice(&[0x81, 0x83]);
        assert_eq!(parse(&missing, 3), Some(Answer::NoSuchName));
        let mut refused = query.clone();
        refused[2..4].copy_from_slice(&[0x81, 0x85]);
        assert_eq!(parse(&refused, 3), Some(Answer::Failed(5)));
        let answer = answer(&query);
        assert_eq!(parse(&answer[..answer.len() - 2], 3), None); // cut short
        let mut looped = answer.clone();
        looped[HEADER] = 0x80; // a label length with reserved bits
        assert_eq!(parse(&looped, 3), None);
    }
}
