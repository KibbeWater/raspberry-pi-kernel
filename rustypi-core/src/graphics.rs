// graphics.rs
//! Colours and how they are laid out in a pixel.

use crate::mailbox::tags::PixelOrder;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const BLACK: Color = Color::rgb(0, 0, 0);
    pub const WHITE: Color = Color::rgb(255, 255, 255);
    pub const RED: Color = Color::rgb(255, 0, 0);
    pub const GREEN: Color = Color::rgb(0, 255, 0);
    pub const BLUE: Color = Color::rgb(0, 0, 255);
    pub const LIGHT_GRAY: Color = Color::rgb(200, 200, 200);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Color { r, g, b }
    }

    /// From a `0x00RRGGBB` word, the layout programs draw with. The top byte is ignored.
    pub const fn from_rgb(word: u32) -> Self {
        Color { r: (word >> 16) as u8, g: (word >> 8) as u8, b: word as u8 }
    }

    /// The 32-bit pixel for this colour. `RGB` puts red in the lowest byte, `BGR` blue; the
    /// top byte (alpha) is opaque.
    pub fn encode(self, order: PixelOrder) -> u32 {
        let (low, high) = if order == PixelOrder::RGB { (self.r, self.b) } else { (self.b, self.r) };
        0xFF00_0000 | (high as u32) << 16 | (self.g as u32) << 8 | low as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_rgb_words() {
        assert_eq!(Color::from_rgb(0xAA11_2233), Color::rgb(0x11, 0x22, 0x33));
        assert_eq!(Color::from_rgb(0x00FF_8000).encode(PixelOrder::BGR), 0xFFFF_8000);
    }

    #[test]
    fn encodes_channels_in_pixel_order() {
        let c = Color::rgb(0x11, 0x22, 0x33);
        assert_eq!(c.encode(PixelOrder::RGB), 0xFF33_2211);
        assert_eq!(c.encode(PixelOrder::BGR), 0xFF11_2233);
    }
}
