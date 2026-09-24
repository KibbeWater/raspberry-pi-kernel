// update.rs
//! Receiving a new kernel image over UDP, reliably enough to install: the sender announces
//! the size and a CRC-32, sends the image in numbered chunks that are each acknowledged
//! (and sent again until they are), and says when it is done. Only a complete image whose
//! CRC-32 matches comes out.
//!
//! Datagrams, little endian:
//!
//! | from the sender           | answer                                  |
//! |---------------------------|-----------------------------------------|
//! | `B` size:u32 crc:u32      | `K` (begun), or `X` and a reason        |
//! | `D` offset:u32 bytes...   | `A` offset:u32, or `X` and a reason     |
//! | `E`                       | the image comes out (the kernel answers)|

use alloc::vec;
use alloc::vec::Vec;

/// The UDP port updates arrive on.
pub const PORT: u16 = 2324;
/// Bytes of image in a data datagram, at most: every chunk but the last is this long.
pub const CHUNK: usize = 1024;
/// The biggest image taken: far more than a kernel needs, far less than the heap.
pub const MAX_IMAGE: usize = 8 * 1024 * 1024;

/// CRC-32 (IEEE 802.3, the one zip and Ethernet use) of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { crc >> 1 ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// What to do with a datagram's result.
#[derive(Debug, PartialEq, Eq)]
pub enum Response {
    /// Answer the sender with this.
    Reply(Vec<u8>),
    /// The whole image arrived and its CRC-32 matches: install it, and answer the sender.
    Complete(Vec<u8>),
}

fn refuse(reason: &str) -> Response {
    let mut reply = vec![b'X'];
    reply.extend_from_slice(reason.as_bytes());
    Response::Reply(reply)
}

struct Transfer {
    crc: u32,
    image: Vec<u8>,
    /// Which chunks have arrived.
    arrived: Vec<bool>,
    missing: usize,
}

/// Takes one update at a time.
#[derive(Default)]
pub struct Receiver {
    transfer: Option<Transfer>,
}

impl Receiver {
    pub const fn new() -> Self {
        Receiver { transfer: None }
    }

    /// Forgets an update in progress.
    pub fn reset(&mut self) {
        self.transfer = None;
    }

    /// Takes a datagram from the sender.
    pub fn handle(&mut self, datagram: &[u8]) -> Response {
        let word = |at: usize| datagram.get(at..at + 4).map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()));
        match datagram.first() {
            Some(b'B') => {
                let (Some(size), Some(crc)) = (word(1), word(5)) else { return refuse("short begin") };
                let size = size as usize;
                if size == 0 || size > MAX_IMAGE {
                    self.transfer = None;
                    return refuse("bad size");
                }
                let chunks = size.div_ceil(CHUNK);
                self.transfer = Some(Transfer { crc, image: vec![0; size], arrived: vec![false; chunks], missing: chunks });
                Response::Reply(vec![b'K'])
            }
            Some(b'D') => {
                let Some(transfer) = self.transfer.as_mut() else { return refuse("not begun") };
                let Some(offset) = word(1) else { return refuse("short data") };
                let (offset, bytes) = (offset as usize, &datagram[5..]);
                // Chunks start on chunk boundaries and fill them, all but the last.
                let expected = transfer.image.len().saturating_sub(offset).min(CHUNK);
                if offset % CHUNK != 0 || offset >= transfer.image.len() || bytes.len() != expected {
                    return refuse("bad chunk");
                }
                let index = offset / CHUNK;
                if !transfer.arrived[index] {
                    transfer.image[offset..offset + bytes.len()].copy_from_slice(bytes);
                    transfer.arrived[index] = true;
                    transfer.missing -= 1;
                }
                let mut ack = vec![b'A'];
                ack.extend_from_slice(&(offset as u32).to_le_bytes());
                Response::Reply(ack)
            }
            Some(b'E') => {
                let Some(transfer) = self.transfer.take() else { return refuse("not begun") };
                if transfer.missing != 0 {
                    self.transfer = Some(transfer);
                    return refuse("chunks missing");
                }
                if crc32(&transfer.image) != transfer.crc {
                    return refuse("CRC-32 mismatch");
                }
                Response::Complete(transfer.image)
            }
            _ => refuse("unknown"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn begin(size: u32, crc: u32) -> Vec<u8> {
        let mut datagram = vec![b'B'];
        datagram.extend_from_slice(&size.to_le_bytes());
        datagram.extend_from_slice(&crc.to_le_bytes());
        datagram
    }

    fn data(offset: usize, bytes: &[u8]) -> Vec<u8> {
        let mut datagram = vec![b'D'];
        datagram.extend_from_slice(&(offset as u32).to_le_bytes());
        datagram.extend_from_slice(bytes);
        datagram
    }

    fn ack(offset: usize) -> Response {
        let mut reply = vec![b'A'];
        reply.extend_from_slice(&(offset as u32).to_le_bytes());
        Response::Reply(reply)
    }

    fn refused(reason: &str) -> Response {
        refuse(reason)
    }

    /// An image of `len` bytes that isn't all one value.
    fn image(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn an_image_sent_in_chunks_comes_out_whole() {
        let image = image(2 * CHUNK + 100);
        let mut receiver = Receiver::new();
        assert_eq!(receiver.handle(&begin(image.len() as u32, crc32(&image))), Response::Reply(vec![b'K']));
        // Out of order, and one sent twice: fine.
        for offset in [CHUNK, 0, 2 * CHUNK, CHUNK] {
            let end = (offset + CHUNK).min(image.len());
            assert_eq!(receiver.handle(&data(offset, &image[offset..end])), ack(offset));
        }
        assert_eq!(receiver.handle(b"E"), Response::Complete(image));
    }

    #[test]
    fn an_incomplete_image_isnt_let_out_until_it_is_whole() {
        let image = image(CHUNK + 1);
        let mut receiver = Receiver::new();
        receiver.handle(&begin(image.len() as u32, crc32(&image)));
        receiver.handle(&data(0, &image[..CHUNK]));
        assert_eq!(receiver.handle(b"E"), refused("chunks missing"));
        // The transfer carries on: the missing chunk, then done.
        receiver.handle(&data(CHUNK, &image[CHUNK..]));
        assert_eq!(receiver.handle(b"E"), Response::Complete(image));
    }

    #[test]
    fn a_crc_mismatch_is_refused_and_the_transfer_dropped() {
        let image = image(100);
        let mut receiver = Receiver::new();
        receiver.handle(&begin(100, crc32(&image) ^ 1));
        receiver.handle(&data(0, &image));
        assert_eq!(receiver.handle(b"E"), refused("CRC-32 mismatch"));
        assert_eq!(receiver.handle(b"E"), refused("not begun"));
    }

    #[test]
    fn bad_datagrams_are_refused() {
        let mut receiver = Receiver::new();
        assert_eq!(receiver.handle(&data(0, b"x")), refused("not begun"));
        assert_eq!(receiver.handle(&begin(0, 0)), refused("bad size"));
        assert_eq!(receiver.handle(&begin(MAX_IMAGE as u32 + 1, 0)), refused("bad size"));
        assert_eq!(receiver.handle(b"B12"), refused("short begin"));
        receiver.handle(&begin(2 * CHUNK as u32, 0));
        assert_eq!(receiver.handle(&data(1, &[0; CHUNK])), refused("bad chunk")); // not on a boundary
        assert_eq!(receiver.handle(&data(0, &[0; 10])), refused("bad chunk")); // short, not the last
        assert_eq!(receiver.handle(&data(2 * CHUNK, &[0; 1])), refused("bad chunk")); // past the end
        assert_eq!(receiver.handle(b"?"), refused("unknown"));
        assert_eq!(receiver.handle(b""), refused("unknown"));
    }

    #[test]
    fn beginning_again_starts_over() {
        let image = image(10);
        let mut receiver = Receiver::new();
        receiver.handle(&begin(10, 0));
        receiver.handle(&data(0, &[0; 10]));
        receiver.handle(&begin(10, crc32(&image)));
        assert_eq!(receiver.handle(b"E"), refused("chunks missing"));
        receiver.handle(&data(0, &image));
        assert_eq!(receiver.handle(b"E"), Response::Complete(image));
    }
}
