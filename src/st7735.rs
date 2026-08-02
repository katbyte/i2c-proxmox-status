//! Driver for the UCTRONICS SKU_RM0004 front panel: an ST7735 160x80 LCD
//! sitting behind an RP2040 that bridges I2C (address 0x18) to the panel.
//!
//! Ported from `hardware/st7735/st7735.c` (see git history). The register
//! protocol is specific to this board's bridge firmware, not the raw ST7735
//! command set: every transaction is `[register, high_byte, low_byte]`, plus
//! a burst mode for bulk pixel pushes.
//!
//! All drawing happens in `FrameBuffer`; this module is just the transport.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::thread;
use std::time::Duration;

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
const BURST_WRITE_REG: u8 = 0x01;
const SYNC_REG: u8 = 0x03;

const BURST_MAX_LENGTH: usize = 160;

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

    /// Draw a w*h block of RGB565 pixels at (x, y).
    ///
    /// Split into windows of at most MAX_SESSION_BYTES each: longer burst
    /// sessions overrun the bridge firmware and the content lands displaced
    /// on the panel. 16 full-width rows is the largest session verified on
    /// real hardware.
    pub fn blit(&mut self, x: u16, y: u16, w: u16, h: u16, pixels: &[u16]) -> io::Result<()> {
        const MAX_SESSION_BYTES: usize = 16 * 160 * 2;

        let pixels = &pixels[..w as usize * h as usize];
        let session_rows = (MAX_SESSION_BYTES / (2 * w as usize)).max(1) as u16;
        let mut row = 0;
        while row < h {
            let rows = session_rows.min(h - row);
            self.set_address_window(
                x as u8,
                (y + row) as u8,
                (x + w - 1) as u8,
                (y + row + rows - 1) as u8,
            )?;
            let span = &pixels[row as usize * w as usize..(row + rows) as usize * w as usize];
            let bytes: Vec<u8> = span.iter().flat_map(|p| p.to_be_bytes()).collect();
            self.burst_transfer(&bytes)?;
            row += rows;
        }
        Ok(())
    }
}
