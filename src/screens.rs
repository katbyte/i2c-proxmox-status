//! The header marquee and the rotating status screens.
//!
//! The static background (black fill + blue divider bar) is painted once at
//! startup by main; the header and each screen only ever repaint their own
//! region, so the display never visibly wipes.
//!
//! Everything is rasterized into offscreen RGB565 buffers and pushed with
//! burst-mode blits: the I2C bus (~40 KB/s at 400kHz) is the bottleneck, and
//! per-pixel writes are an order of magnitude slower than bursts.

use std::io;

use crate::fonts::{Font, FONT_11X18, FONT_8X16};
use crate::st7735::{self, Lcd};
use crate::stats;

pub trait Screen {
    fn render(&mut self, lcd: &mut Lcd) -> io::Result<()>;
}

/// Draw `text` into an offscreen buffer at pixel offset x, clipping at the
/// buffer's right edge.
fn draw_text(buf: &mut [u16], buf_w: usize, x: usize, text: &str, font: &Font, color: u16) {
    let fw = font.width as usize;
    for (i, ch) in text.chars().enumerate() {
        let x0 = x + i * fw;
        if x0 + fw > buf_w {
            break;
        }
        for (row, &bits) in font.glyph(ch).iter().enumerate() {
            for bit in 0..fw {
                if (bits << bit) & 0x8000 != 0 {
                    buf[row * buf_w + x0 + bit] = color;
                }
            }
        }
    }
}

/// Top-of-screen "ip - hostname.fqdn" line. Text wider than the display
/// scrolls as a marquee; shorter text is drawn once and left alone.
/// One frame is a ~5KB blit (~140ms on the bus), so ~7fps is the ceiling —
/// scroll in 1px steps to look as smooth as the bus allows.
pub struct Header {
    text: Vec<char>,
    offset: usize,
    dirty: bool,
}

const HEADER_HEIGHT: u16 = 16; // FONT_8X16
const SCROLL_GAP_PX: usize = 24; // blank run between marquee repeats
const SCROLL_STEP_PX: usize = 1;

impl Header {
    pub fn new(text: &str) -> Self {
        Self { text: text.chars().collect(), offset: 0, dirty: true }
    }

    fn pixel_width(&self) -> usize {
        self.text.len() * FONT_8X16.width as usize
    }

    fn scrolling(&self) -> bool {
        self.pixel_width() > st7735::WIDTH as usize
    }

    /// Advance the marquee one step and repaint if anything moved.
    pub fn tick(&mut self, lcd: &mut Lcd) -> io::Result<()> {
        if self.scrolling() {
            self.offset = (self.offset + SCROLL_STEP_PX) % (self.pixel_width() + SCROLL_GAP_PX);
            self.dirty = true;
        }
        self.render(lcd)
    }

    /// Rasterize the visible slice of the text and blit it in one burst.
    pub fn render(&mut self, lcd: &mut Lcd) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let font = &FONT_8X16;
        let font_width = font.width as usize;
        let (w, h) = (st7735::WIDTH as usize, font.height as usize);
        let span = self.pixel_width() + SCROLL_GAP_PX;

        let mut buf = vec![st7735::BLACK; w * h];
        for col in 0..w {
            let src = if self.scrolling() { (self.offset + col) % span } else { col };
            if src >= self.pixel_width() {
                continue; // in the gap between marquee repeats / past short text
            }
            let rows = font.glyph(self.text[src / font_width]);
            let bit = src % font_width;
            for (row, &bits) in rows.iter().enumerate() {
                if (bits << bit) & 0x8000 != 0 {
                    buf[row * w + col] = st7735::WHITE;
                }
            }
        }
        lcd.blit(0, 0, st7735::WIDTH, HEADER_HEIGHT, &buf)?;
        self.dirty = false;
        Ok(())
    }
}

/// "LABEL: <value><unit>" in the large font, blitted as one band.
fn draw_value_line(lcd: &mut Lcd, label: &str, x: u16, value: u8, unit: &str) -> io::Result<()> {
    let (w, h) = (st7735::WIDTH as usize, FONT_11X18.height as usize);
    let mut buf = vec![st7735::BLACK; w * h];
    let text = format!("{label}{value}{unit}");
    draw_text(&mut buf, w, x as usize, &text, &FONT_11X18, st7735::WHITE);
    lcd.blit(0, 35, st7735::WIDTH, FONT_11X18.height, &buf)
}

/// Ten-segment bar gauge, blitted as one 100x10 band at (30, 60).
fn draw_gauge(lcd: &mut Lcd, percent: u8, color: u16) -> io::Result<()> {
    const SEG_W: usize = 6;
    const SEG_PITCH: usize = 10;
    const BAND_W: usize = 10 * SEG_PITCH;
    const BAND_H: usize = 10;

    let filled = ((percent.min(100) as usize + 10).min(100)) / 10;
    let mut buf = vec![st7735::BLACK; BAND_W * BAND_H];
    for segment in 0..10 {
        let segment_color = if segment < filled { color } else { st7735::GRAY };
        for row in 0..BAND_H {
            let start = row * BAND_W + segment * SEG_PITCH;
            buf[start..start + SEG_W].fill(segment_color);
        }
    }
    lcd.blit(30, 60, BAND_W as u16, BAND_H as u16, &buf)
}

pub struct CpuScreen {
    sampler: stats::CpuSampler,
}

impl CpuScreen {
    pub fn new() -> Self {
        Self { sampler: stats::CpuSampler::new() }
    }
}

impl Screen for CpuScreen {
    fn render(&mut self, lcd: &mut Lcd) -> io::Result<()> {
        let cpu = self.sampler.percent()?;
        draw_value_line(lcd, "CPU:", 36, cpu, "%")?;
        draw_gauge(lcd, cpu, st7735::GREEN)
    }
}

pub struct RamScreen;

impl Screen for RamScreen {
    fn render(&mut self, lcd: &mut Lcd) -> io::Result<()> {
        let ram = stats::memory_percent()?;
        draw_value_line(lcd, "RAM:", 36, ram, "%")?;
        draw_gauge(lcd, ram, st7735::YELLOW)
    }
}

pub struct TempScreen;

impl Screen for TempScreen {
    fn render(&mut self, lcd: &mut Lcd) -> io::Result<()> {
        let temp = stats::temperature_celsius()?;
        draw_value_line(lcd, "TEMP:", 30, temp, "C")?;
        draw_gauge(lcd, temp, st7735::RED)
    }
}

pub struct DiskScreen;

impl Screen for DiskScreen {
    fn render(&mut self, lcd: &mut Lcd) -> io::Result<()> {
        let disk = stats::disk_percent()?;
        draw_value_line(lcd, "DISK:", 30, disk, "%")?;
        draw_gauge(lcd, disk, st7735::BLUE)
    }
}
