//! Drawing on the screen. Opening it takes it from the kernel console, which comes back when
//! the `Screen` is dropped or the program exits.

extern crate alloc;

use alloc::vec;
use rustypi_abi::{Errno, ScreenSize, Syscall};
use crate::{syscall, Handle};

/// A colour as programs draw it: `0x00RRGGBB`.
pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// The screen, while this program has it.
pub struct Screen {
    handle: Handle,
    size: ScreenSize,
}

/// Takes the screen. `Busy` if another program has it, `NoDevice` without a display.
pub fn open() -> Result<Screen, Errno> {
    let mut size = ScreenSize::default();
    let handle = syscall::call(Syscall::OpenScreen { size: &mut size as *mut ScreenSize as u64 })?;
    Ok(Screen { handle: Handle(handle), size })
}

impl Screen {
    pub fn width(&self) -> usize {
        self.size.width as usize
    }

    pub fn height(&self) -> usize {
        self.size.height as usize
    }

    /// Draws `width` by `pixels.len() / width` pixels, row by row, with the top left corner
    /// at (`x`, `y`). Whatever falls off the screen is left out.
    pub fn draw(&mut self, x: usize, y: usize, width: usize, pixels: &[u32]) -> Result<(), Errno> {
        if width == 0 {
            return Ok(());
        }
        let call = Syscall::Draw {
            handle: self.handle.0,
            x: x as u64,
            y: y as u64,
            width: width as u64,
            height: (pixels.len() / width) as u64,
            pixels: pixels.as_ptr() as u64,
        };
        syscall::call(call).map(|_| ())
    }

    /// Fills a rectangle with one colour.
    pub fn fill(&mut self, x: usize, y: usize, width: usize, height: usize, color: u32) -> Result<(), Errno> {
        let row = vec![color; width];
        for row_y in y..y + height {
            self.draw(x, row_y, width, &row)?;
        }
        Ok(())
    }

    pub fn clear(&mut self, color: u32) -> Result<(), Errno> {
        self.fill(0, 0, self.width(), self.height(), color)
    }
}
