// net/lan7800.rs
//! How the LAN7800 (the Pi 3 B+'s Ethernet controller) wraps frames on its USB bulk
//! endpoints: an 8-byte command header on each frame sent; and on those received, a 10-byte
//! header, the frame with its FCS, and padding, several of them to a transfer.

use alloc::vec::Vec;

/// TX command A: the frame's length, and "add the FCS". Command B, 0, follows.
const TX_CMD_A_FCS: u32 = 1 << 22;
const TX_CMD_A_LEN: u32 = 0x000F_FFFF;
/// RX command A: "receive error", and the frame's length with its FCS. B and C follow.
const RX_CMD_A_ERROR: u32 = 1 << 22;
const RX_CMD_A_LEN: u32 = 0x3FFF;
const RX_HEADER: usize = 4 + 4 + 2;
const FCS: usize = 4;

/// `frame` as the bulk OUT endpoint takes it.
pub fn tx(frame: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8 + frame.len());
    bytes.extend_from_slice(&((frame.len() as u32 & TX_CMD_A_LEN) | TX_CMD_A_FCS).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(frame);
    bytes
}

/// The frames in what one bulk IN transfer brought, without their FCS. Frames the controller
/// marks bad are left out; a transfer cut short ends the list.
pub fn rx(transfer: &[u8]) -> Vec<&[u8]> {
    let mut frames = Vec::new();
    let mut at = 0;
    while at + RX_HEADER <= transfer.len() {
        let cmd_a = u32::from_le_bytes(transfer[at..at + 4].try_into().unwrap());
        let size = (cmd_a & RX_CMD_A_LEN) as usize;
        let start = at + RX_HEADER;
        if start + size > transfer.len() {
            break;
        }
        if cmd_a & RX_CMD_A_ERROR == 0 && size > FCS {
            frames.push(&transfer[start..start + size - FCS]);
        }
        // The next frame starts 4-byte aligned, counting the header's 2-byte C word.
        let padding = (4 - (size + 2) % 4) % 4;
        at = start + size + padding;
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame as the controller hands it over: header, frame, 4-byte FCS, padding.
    fn received(frame: &[u8], error: bool) -> Vec<u8> {
        let size = frame.len() + FCS;
        let cmd_a = size as u32 | if error { RX_CMD_A_ERROR } else { 0 };
        let mut bytes = cmd_a.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0; 6]);
        bytes.extend_from_slice(frame);
        bytes.extend_from_slice(&[0xFC; FCS]);
        bytes.resize(bytes.len() + (4 - (size + 2) % 4) % 4, 0xEE);
        bytes
    }

    #[test]
    fn frames_sent_get_their_header() {
        let bytes = tx(&[1, 2, 3]);
        assert_eq!(&bytes[..8], &[3, 0, 0x40, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[8..], &[1, 2, 3]);
    }

    #[test]
    fn several_frames_in_a_transfer_come_apart() {
        let mut transfer = received(&[1; 60], false);
        transfer.extend(received(&[2; 61], false));
        transfer.extend(received(&[3; 63], true)); // bad: left out
        transfer.extend(received(&[4; 64], false));
        let frames = rx(&transfer);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0], &[1; 60]);
        assert_eq!(frames[1], &[2; 61]);
        assert_eq!(frames[2], &[4; 64]);
    }

    #[test]
    fn empty_and_cut_transfers_give_what_is_whole() {
        assert!(rx(&[]).is_empty());
        let whole = received(&[7; 60], false);
        let mut transfer = whole.clone();
        transfer.extend_from_slice(&received(&[8; 60], false)[..30]);
        assert_eq!(rx(&transfer), [&[7; 60][..]]);
    }
}
