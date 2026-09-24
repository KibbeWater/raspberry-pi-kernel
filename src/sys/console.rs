// console.rs
//! The screen console: `rustypi_core::console::Console` drawn on the framebuffer. Mirrors
//! everything `print!` writes.

use core::fmt;
use rustypi_core::console::{Console, Surface};
use rustypi_core::font::GLYPH_SIZE;
use rustypi_core::graphics::Color;
use crate::drivers::framebuffer::{Framebuffer, FramebufferError};
use crate::synchronization::TryLock;

const FOREGROUND: Color = Color::LIGHT_GRAY;
const BACKGROUND: Color = Color::BLACK;

struct Screen {
    framebuffer: Framebuffer,
    console: Console,
    /// Font pixels are `scale` screen pixels wide, so text stays readable on large screens.
    scale: usize,
}

/// Draws console cells on the framebuffer.
struct Cells<'a> {
    framebuffer: &'a mut Framebuffer,
    scale: usize,
}

impl Surface for Cells<'_> {
    fn draw_cell(&mut self, col: usize, row: usize, c: char) {
        let cell = GLYPH_SIZE * self.scale;
        self.framebuffer.draw_char(col * cell, row * cell, c, self.scale, FOREGROUND, BACKGROUND);
    }
}

impl Screen {
    fn with_cells<R>(&mut self, f: impl FnOnce(&mut Console, &mut Cells) -> R) -> R {
        let mut cells = Cells { framebuffer: &mut self.framebuffer, scale: self.scale };
        f(&mut self.console, &mut cells)
    }
}

/// Only touched through `try_lock`: output that arrives while the screen is busy (from a
/// panic in the middle of drawing, say) goes to the UART only.
static SCREEN: TryLock<Option<Screen>> = TryLock::new(None);

/// Allocates the framebuffer and starts mirroring output to it.
pub fn init() -> Result<(), FramebufferError> {
    let mut framebuffer = Framebuffer::new()?;
    let scale = if framebuffer.width() >= 1280 { 2 } else { 1 };
    let cell = GLYPH_SIZE * scale;
    let console = Console::new(framebuffer.width() / cell, framebuffer.height() / cell);
    framebuffer.clear(BACKGROUND);
    SCREEN.try_lock(|screen| *screen = Some(Screen { framebuffer, console, scale }));
    Ok(())
}

/// Writes to the screen, if there is one and it isn't busy.
pub fn write_fmt(args: fmt::Arguments) {
    struct Writer<'a>(&'a mut Screen);

    impl fmt::Write for Writer<'_> {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            self.0.with_cells(|console, cells| console.write_str(s, cells));
            Ok(())
        }
    }

    SCREEN.try_lock(|screen| {
        if let Some(screen) = screen {
            let _ = fmt::Write::write_fmt(&mut Writer(screen), args);
        }
    });
}

pub struct Info {
    pub width: usize,
    pub height: usize,
    pub pitch: usize,
    pub address: usize,
    pub bytes: usize,
    pub rgb: bool,
    pub cols: usize,
    pub rows: usize,
}

/// The screen's layout, if there is one.
pub fn info() -> Option<Info> {
    SCREEN.try_lock(|screen| {
        screen.as_ref().map(|s| Info {
            width: s.framebuffer.width(),
            height: s.framebuffer.height(),
            pitch: s.framebuffer.pitch(),
            address: s.framebuffer.address(),
            bytes: s.framebuffer.bytes(),
            rgb: s.framebuffer.pixel_order() == crate::drivers::mailbox::tags::PixelOrder::RGB,
            cols: s.console.cols(),
            rows: s.console.rows(),
        })
    })
    .flatten()
}

/// Draws red, green, blue and white bars over the whole screen, to check the colours come
/// out right. Returns false if there is no screen.
pub fn test_pattern() -> bool {
    const BARS: [Color; 4] = [Color::RED, Color::GREEN, Color::BLUE, Color::WHITE];
    SCREEN
        .try_lock(|screen| {
            let Some(screen) = screen else { return false };
            let fb = &mut screen.framebuffer;
            let bar = fb.width() / BARS.len();
            for (i, color) in BARS.into_iter().enumerate() {
                fb.fill_rect(i * bar, 0, bar, fb.height(), color);
            }
            true
        })
        .unwrap_or(false)
}

/// Draws the console's text again, e.g. after `test_pattern`.
pub fn redraw() -> bool {
    SCREEN
        .try_lock(|screen| {
            let Some(screen) = screen else { return false };
            screen.framebuffer.clear(BACKGROUND);
            screen.with_cells(|console, cells| console.redraw(cells));
            true
        })
        .unwrap_or(false)
}
