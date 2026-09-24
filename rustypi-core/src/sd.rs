// sd.rs
//! Decoding SD card registers.

/// The card's capacity in 512-byte blocks from its CSD register (the standard 128-bit
/// layout), or `None` for an unknown CSD structure version.
pub fn capacity_blocks(csd: u128) -> Option<u64> {
    let bits = |high: u32, low: u32| ((csd >> low) & ((1u128 << (high - low + 1)) - 1)) as u64;
    match bits(127, 126) {
        // CSD 2.0 (SDHC/SDXC): (C_SIZE + 1) * 512KB.
        1 => Some((bits(69, 48) + 1) * 1024),
        // CSD 1.0: (C_SIZE + 1) * 2^(C_SIZE_MULT + 2) blocks of 2^READ_BL_LEN bytes.
        0 => Some(((bits(73, 62) + 1) << (bits(49, 47) + 2)) << bits(83, 80) >> 9),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_csd_version_2() {
        // A 32GB SDHC card: C_SIZE 61055.
        assert_eq!(capacity_blocks(0x400e_0032_5b59_0000_ee7f_7f80_0a40_40df), Some(62_521_344));
    }

    #[test]
    fn decodes_csd_version_1() {
        // A standard capacity card: C_SIZE 3863, C_SIZE_MULT 5, 1024-byte READ_BL_LEN.
        assert_eq!(capacity_blocks(0x005e_0032_5f5a_83c5_ecb6_db7f_9280_00c9), Some(989_184));
    }

    #[test]
    fn unknown_structure_versions_are_none() {
        assert_eq!(capacity_blocks(0xC000_0000_0000_0000_0000_0000_0000_0000), None);
    }
}
