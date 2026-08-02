//! Driver for the UCTRONICS SKU_RM0004 front panel: an ST7735 160x80 LCD
//! sitting behind an RP2040 that bridges I2C (address 0x18) to the panel.
//!
//! Ported from `hardware/st7735/st7735.c`. The register protocol is specific
//! to this board's bridge firmware, not the raw ST7735 command set: every
//! transaction is `[register, high_byte, low_byte]`, plus a burst mode for
//! bulk pixel pushes.

// The full API of the C driver is kept, even the parts no screen uses yet.
#![allow(dead_code)]

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::thread;
use std::time::Duration;

use crate::fonts::Font;

pub const WIDTH: u16 = 160;
pub const HEIGHT: u16 = 80;

// Panel offset for the 160x80 module in "rotate right" orientation.
const X_START: u8 = 0;
const Y_START: u8 = 24;

const I2C_DEVICE: &str = "/dev/i2c-1";
const I2C_ADDRESS: libc::c_ulong = 0x18;
const I2C_SLAVE_FORCE: libc::c_ulong = 0x0706;

// Bridge registers.
const X_COORDINATE_REG: u8 = 0x2A;
const Y_COORDINATE_REG: u8 = 0x2B;
const CHAR_DATA_REG: u8 = 0x2C;
const WRITE_DATA_REG: u8 = 0x00;
const BURST_WRITE_REG: u8 = 0x01;
const SYNC_REG: u8 = 0x03;

const BURST_MAX_LENGTH: usize = 160;

// RGB565 colors.
pub const BLACK: u16 = 0x0000;
pub const BLUE: u16 = 0x001F;
pub const RED: u16 = 0xF800;
pub const GREEN: u16 = 0x07E0;
pub const CYAN: u16 = 0x07FF;
pub const MAGENTA: u16 = 0xF81F;
pub const YELLOW: u16 = 0xFFE0;
pub const WHITE: u16 = 0xFFFF;
pub const GRAY: u16 = 0x8410;

pub const fn color565(r: u8, g: u8, b: u8) -> u16 {
    ((r as u16 & 0xF8) << 8) | ((g as u16 & 0xFC) << 3) | ((b as u16 & 0xF8) >> 3)
}

pub struct Lcd {
    i2c: File,
}

impl Lcd {
    /// Open /dev/i2c-1 and select the display bridge as the slave device.
    pub fn open() -> io::Result<Self> {
        let i2c = File::options().read(true).write(true).open(I2C_DEVICE)?;
        let rc = unsafe { libc::ioctl(i2c.as_raw_fd(), I2C_SLAVE_FORCE, I2C_ADDRESS) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { i2c })
    }

    fn write_command(&mut self, command: u8, high: u8, low: u8) -> io::Result<()> {
        self.i2c.write_all(&[command, high, low])?;
        thread::sleep(Duration::from_micros(10));
        Ok(())
    }

    fn write_data(&mut self, high: u8, low: u8) -> io::Result<()> {
        self.write_command(WRITE_DATA_REG, high, low)
    }

    /// Push a pixel buffer through the bridge's burst mode, chunked to its
    /// 160-byte limit with the settle delay the firmware needs between chunks.
    fn burst_transfer(&mut self, data: &[u8]) -> io::Result<()> {
        self.write_command(BURST_WRITE_REG, 0x00, 0x01)?;
        for chunk in data.chunks(BURST_MAX_LENGTH) {
            self.i2c.write_all(chunk)?;
            thread::sleep(Duration::from_micros(700));
        }
        self.write_command(BURST_WRITE_REG, 0x00, 0x00)?;
        self.write_command(SYNC_REG, 0x00, 0x01)
    }

    fn set_address_window(&mut self, x0: u8, y0: u8, x1: u8, y1: u8) -> io::Result<()> {
        self.write_command(X_COORDINATE_REG, x0 + X_START, x1 + X_START)?;
        self.write_command(Y_COORDINATE_REG, y0 + Y_START, y1 + Y_START)?;
        self.write_command(CHAR_DATA_REG, 0x00, 0x00)?;
        self.write_command(SYNC_REG, 0x00, 0x01)
    }

    pub fn write_char(
        &mut self,
        x: u16,
        y: u16,
        ch: char,
        font: &Font,
        color: u16,
        bgcolor: u16,
    ) -> io::Result<()> {
        self.set_address_window(
            x as u8,
            y as u8,
            (x + font.width - 1) as u8,
            (y + font.height - 1) as u8,
        )?;
        for &row in font.glyph(ch) {
            for bit in 0..font.width {
                let pixel = if (row << bit) & 0x8000 != 0 { color } else { bgcolor };
                self.write_data((pixel >> 8) as u8, (pixel & 0xFF) as u8)?;
            }
        }
        Ok(())
    }

    /// Draw a string, wrapping at the right edge and stopping at the bottom.
    pub fn write_string(
        &mut self,
        x: u16,
        y: u16,
        text: &str,
        font: &Font,
        color: u16,
        bgcolor: u16,
    ) -> io::Result<()> {
        let (mut x, mut y) = (x, y);
        for ch in text.chars() {
            if x + font.width >= WIDTH {
                x = 0;
                y += font.height;
                if y + font.height >= HEIGHT {
                    break;
                }
                if ch == ' ' {
                    continue; // skip spaces at the start of a wrapped line
                }
            }
            self.write_char(x, y, ch, font, color, bgcolor)?;
            self.write_command(SYNC_REG, 0x00, 0x01)?;
            x += font.width;
        }
        Ok(())
    }

    pub fn fill_rectangle(&mut self, x: u16, y: u16, w: u16, h: u16, color: u16) -> io::Result<()> {
        if x >= WIDTH || y >= HEIGHT {
            return Ok(());
        }
        let w = w.min(WIDTH - x);
        let h = h.min(HEIGHT - y);
        self.set_address_window(x as u8, y as u8, (x + w - 1) as u8, (y + h - 1) as u8)?;

        let row: Vec<u8> = (0..w)
            .flat_map(|_| [(color >> 8) as u8, (color & 0xFF) as u8])
            .collect();
        for _ in 0..h {
            self.burst_transfer(&row)?;
        }
        Ok(())
    }

    pub fn fill_screen(&mut self, color: u16) -> io::Result<()> {
        self.fill_rectangle(0, 0, WIDTH, HEIGHT, color)?;
        self.write_command(SYNC_REG, 0x00, 0x01)
    }

    /// Draw a w*h block of big-endian RGB565 pixel data at (x, y).
    pub fn draw_image(&mut self, x: u16, y: u16, w: u16, h: u16, data: &[u8]) -> io::Result<()> {
        self.set_address_window(x as u8, y as u8, (x + w - 1) as u8, (y + h - 1) as u8)?;
        self.burst_transfer(&data[..2 * w as usize * h as usize])
    }
}
