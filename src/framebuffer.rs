//! In-memory framebuffer with differential flushing.
//!
//! Everything draws into `back` (plain RAM, effectively free), and `flush`
//! compares it against `shown` — a copy of what's currently on the glass —
//! pushing only the changed rectangles over I2C. The bus (~40 KB/s) is the
//! bottleneck, so the less we send, the smoother everything looks.
//!
//! Implements embedded-graphics' `DrawTarget`, so all of its primitives
//! (text, rectangles, images, ...) render into this buffer.

use std::convert::Infallible;
use std::io;

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;

use crate::st7735::{Lcd, HEIGHT, WIDTH};

const W: usize = WIDTH as usize;
const H: usize = HEIGHT as usize;

pub struct FrameBuffer {
    back: Vec<u16>,  // RGB565, what the next flush should show
    shown: Vec<u16>, // RGB565, what the display currently shows
}

impl FrameBuffer {
    pub fn new() -> Self {
        Self {
            back: vec![0; W * H],
            // Deliberately different from `back` so the first flush pushes
            // the full frame, whatever the display had on it before.
            shown: vec![1; W * H],
        }
    }

    /// Push every region where `back` differs from `shown` to the display,
    /// top to bottom: contiguous runs of changed rows are sent as one
    /// full-width blit.
    ///
    /// Bands are deliberately NOT trimmed to the dirty columns: the bridge
    /// MCU has only ever been proven with full-width windows (that's all the
    /// original C code used), and narrow windows at arbitrary offsets showed
    /// displaced/stale pixels on real hardware. A full 160px row is 320
    /// bytes (~9ms), so the saving wasn't worth the risk anyway.
    pub fn flush(&mut self, lcd: &mut Lcd) -> io::Result<()> {
        let mut y = 0;
        while y < H {
            if !self.row_dirty(y) {
                y += 1;
                continue;
            }
            let band_start = y;
            while y < H && self.row_dirty(y) {
                y += 1;
            }

            let band = band_start * W..y * W;
            lcd.blit(
                0,
                band_start as u16,
                WIDTH,
                (y - band_start) as u16,
                &self.back[band.clone()],
            )?;
            self.shown[band.clone()].copy_from_slice(&self.back[band]);
        }
        Ok(())
    }

    fn row_dirty(&self, y: usize) -> bool {
        self.back[y * W..(y + 1) * W] != self.shown[y * W..(y + 1) * W]
    }
}

impl OriginDimensions for FrameBuffer {
    fn size(&self) -> Size {
        Size::new(WIDTH as u32, HEIGHT as u32)
    }
}

impl DrawTarget for FrameBuffer {
    type Color = Rgb565;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, color) in pixels {
            if (0..W as i32).contains(&point.x) && (0..H as i32).contains(&point.y) {
                self.back[point.y as usize * W + point.x as usize] = color.into_storage();
            }
        }
        Ok(())
    }
}
