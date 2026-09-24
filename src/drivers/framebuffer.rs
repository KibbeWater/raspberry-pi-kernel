// framebuffer.rs
//! HDMI framebuffer, allocated from the GPU firmware through the mailbox.
//!
//! Always 32 bits per pixel. The firmware decides the channel order, so colours go
//! through `Color::encode` with whatever order it granted.

use core::fmt;
use core::ptr::write_volatile;
use rustypi_core::font::{self, GLYPH_SIZE};
use rustypi_core::graphics::Color;
use crate::arch::mmu;
use crate::drivers::mailbox::tags::{
    Alignment, AllocateBuffer, Depth, GetPhysicalSize, GetPitch, Offset, PixelOrder,
    SetDepth, SetPhysicalSize, SetPixelOrder, SetVirtualOffset, SetVirtualSize, Size,
};
use crate::drivers::mailbox::{query, Batch, Mailbox, MailboxError};

/// Used when the firmware reports no display resolution.
const FALLBACK_SIZE: Size = Size { width: 1280, height: 720 };

const DEPTH: Depth = Depth(32);
const BYTES_PER_PIXEL: usize = 4;

#[derive(Debug)]
pub enum FramebufferError {
    Mailbox(MailboxError),
    /// The firmware granted a different depth than the 32 bits asked for.
    Depth(Depth),
    /// The firmware's buffer doesn't fit the resolution it reported.
    Layout { size: Size, pitch: u32, bytes: u32 },
}

impl From<MailboxError> for FramebufferError {
    fn from(error: MailboxError) -> Self {
        FramebufferError::Mailbox(error)
    }
}

impl fmt::Display for FramebufferError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FramebufferError::Mailbox(error) => write!(f, "mailbox: {error}"),
            FramebufferError::Depth(depth) => write!(f, "got {} bits per pixel, need 32", depth.0),
            FramebufferError::Layout { size, pitch, bytes } => write!(
                f,
                "{}x{} with pitch {} doesn't fit in {} bytes",
                size.width, size.height, pitch, bytes,
            ),
        }
    }
}

pub struct Framebuffer {
    base: *mut u32,
    width: usize,
    height: usize,
    /// Pixels (not bytes) from one row to the next.
    stride: usize,
    order: PixelOrder,
    bytes: usize,
}

// The framebuffer memory belongs to this value alone once allocated.
unsafe impl Send for Framebuffer {}

impl Framebuffer {
    /// Allocates a framebuffer at the display's resolution.
    pub fn new() -> Result<Self, FramebufferError> {
        let size = match query::<GetPhysicalSize>(&Mailbox, ()) {
            Ok(size) if size.width > 0 && size.height > 0 => size,
            _ => FALLBACK_SIZE,
        };

        // The allocation uses the settings from the same message.
        let mut batch = Batch::new();
        let physical = batch.add::<SetPhysicalSize>(size);
        let _ = batch.add::<SetVirtualSize>(size);
        let _ = batch.add::<SetVirtualOffset>(Offset { x: 0, y: 0 });
        let depth = batch.add::<SetDepth>(DEPTH);
        let order = batch.add::<SetPixelOrder>(PixelOrder::RGB);
        let allocation = batch.add::<AllocateBuffer>(Alignment(4096));
        let pitch = batch.add::<GetPitch>(());
        let replies = batch.send(&Mailbox)?;

        let depth = replies.get(depth)?;
        if depth != DEPTH {
            return Err(FramebufferError::Depth(depth));
        }
        let size = replies.get(physical)?;
        let allocation = replies.get(allocation)?;
        let pitch = replies.get(pitch)?;
        let (width, height) = (size.width as usize, size.height as usize);
        let pitch_ok = pitch as usize >= width * BYTES_PER_PIXEL && pitch as usize % BYTES_PER_PIXEL == 0;
        if !pitch_ok || (allocation.size as usize) < pitch as usize * height {
            return Err(FramebufferError::Layout { size, pitch, bytes: allocation.size });
        }

        let base = allocation.base.to_arm();
        mmu::make_uncached(base, allocation.size as usize);
        Ok(Framebuffer {
            base: base as *mut u32,
            width,
            height,
            stride: pitch as usize / BYTES_PER_PIXEL,
            order: replies.get(order)?,
            bytes: allocation.size as usize,
        })
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn pitch(&self) -> usize {
        self.stride * BYTES_PER_PIXEL
    }

    pub fn address(&self) -> usize {
        self.base as usize
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn pixel_order(&self) -> PixelOrder {
        self.order
    }

    /// Writes an already encoded pixel. The caller checks the bounds.
    #[inline(always)]
    fn write(&mut self, x: usize, y: usize, pixel: u32) {
        debug_assert!(x < self.width && y < self.height);
        unsafe { write_volatile(self.base.add(y * self.stride + x), pixel) };
    }

    /// Fills a rectangle, clipped to the screen.
    pub fn fill_rect(&mut self, x: usize, y: usize, width: usize, height: usize, color: Color) {
        let pixel = color.encode(self.order);
        let (x_end, y_end) = ((x + width).min(self.width), (y + height).min(self.height));
        for y in y.min(self.height)..y_end {
            for x in x.min(self.width)..x_end {
                self.write(x, y, pixel);
            }
        }
    }

    /// Draws a row of `0x00RRGGBB` pixels starting at (`x`, `y`), clipped to the screen.
    pub fn draw_row(&mut self, x: usize, y: usize, pixels: &[u32]) {
        if y >= self.height {
            return;
        }
        let visible = self.width.saturating_sub(x).min(pixels.len());
        for (i, &pixel) in pixels[..visible].iter().enumerate() {
            let pixel = Color::from_rgb(pixel).encode(self.order);
            self.write(x + i, y, pixel);
        }
    }

    pub fn clear(&mut self, color: Color) {
        self.fill_rect(0, 0, self.width, self.height, color);
    }

    /// Draws `c` with its top left corner at (`x`, `y`), each font pixel `scale` pixels
    /// wide, clipped to the screen.
    pub fn draw_char(&mut self, x: usize, y: usize, c: char, scale: usize, fg: Color, bg: Color) {
        let glyph = font::glyph(c);
        let (fg, bg) = (fg.encode(self.order), bg.encode(self.order));
        for gy in 0..GLYPH_SIZE * scale {
            let py = y + gy;
            if py >= self.height {
                break;
            }
            for gx in 0..GLYPH_SIZE * scale {
                let px = x + gx;
                if px >= self.width {
                    break;
                }
                let set = font::is_set(glyph, gx / scale, gy / scale);
                self.write(px, py, if set { fg } else { bg });
            }
        }
    }
}
