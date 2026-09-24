// net/sntp.rs
//! SNTP (RFC 4330), the simple use of NTP: one request to a time server, one answer with the
//! time. Accurate to about half the round trip, plenty for a clock that says what day it is.

use super::u16_at;

pub const PORT: u16 = 123;
const LENGTH: usize = 48;
/// Seconds from NTP's epoch (1900) to Unix's (1970).
const UNIX_OFFSET: u64 = 2_208_988_800;
/// Leap indicator 0, version 4, mode 3 (client); a server answers in mode 4.
const CLIENT: u8 = 4 << 3 | 3;
const MODE_SERVER: u8 = 4;

/// A request. `cookie` goes in its transmit time; the server copies it back, so the answer
/// can be told from anything else arriving.
pub fn request(cookie: u64) -> [u8; LENGTH] {
    let mut request = [0; LENGTH];
    request[0] = CLIENT;
    request[40..48].copy_from_slice(&cookie.to_be_bytes());
    request
}

/// The time an answer to the request with `cookie` gives, in microseconds since 1970, moved
/// on by half of `round_trip_us` (the time the answer took to come back, about). `None` for
/// anything else: another request's answer, a server refusing (stratum 0, a kiss of death),
/// or not an answer at all.
pub fn parse(bytes: &[u8], cookie: u64, round_trip_us: u64) -> Option<u64> {
    if bytes.len() < LENGTH || bytes[0] & 7 != MODE_SERVER || bytes[1] == 0 {
        return None;
    }
    if bytes[24..32] != cookie.to_be_bytes() {
        return None;
    }
    let seconds = (u16_at(bytes, 40) as u64) << 16 | u16_at(bytes, 42) as u64;
    let fraction = (u16_at(bytes, 44) as u64) << 16 | u16_at(bytes, 46) as u64;
    let unix = seconds.checked_sub(UNIX_OFFSET)?;
    Some(unix * 1_000_000 + ((fraction * 1_000_000) >> 32) + round_trip_us / 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server's answer to `cookie`, saying `unix` seconds and a half.
    fn answer(cookie: u64, unix: u64, stratum: u8) -> [u8; LENGTH] {
        let mut bytes = [0; LENGTH];
        bytes[0] = 4 << 3 | MODE_SERVER;
        bytes[1] = stratum;
        bytes[24..32].copy_from_slice(&cookie.to_be_bytes());
        bytes[40..44].copy_from_slice(&((unix + UNIX_OFFSET) as u32).to_be_bytes());
        bytes[44..48].copy_from_slice(&0x8000_0000u32.to_be_bytes());
        bytes
    }

    #[test]
    fn requests_carry_the_cookie() {
        let request = request(0x1122_3344_5566_7788);
        assert_eq!(request[0], 0x23);
        assert_eq!(&request[40..48], &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
    }

    #[test]
    fn answers_give_the_time_plus_half_the_round_trip() {
        let bytes = answer(7, 1_790_000_000, 2);
        assert_eq!(parse(&bytes, 7, 10_000), Some(1_790_000_000_000_000 + 500_000 + 5_000));
    }

    #[test]
    fn other_answers_and_refusals_are_ignored() {
        assert_eq!(parse(&answer(7, 1_790_000_000, 2), 8, 0), None); // not ours
        assert_eq!(parse(&answer(7, 1_790_000_000, 0), 7, 0), None); // kiss of death
        assert_eq!(parse(&request(7), 7, 0), None); // a request, not an answer
        assert_eq!(parse(&answer(7, 1_790_000_000, 2)[..40], 7, 0), None);
    }
}
