//! The header marquee and the rotating status screens, drawn with
//! embedded-graphics primitives into the framebuffer. Nothing here touches
//! the display — main flushes the framebuffer after composing a frame.

use embedded_graphics::mono_font::ascii::{FONT_10X20, FONT_9X15};
use embedded_graphics::mono_font::{MonoFont, MonoTextStyle};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::text::{Baseline, Text};

use crate::framebuffer::FrameBuffer;
use crate::st7735::WIDTH;
use crate::stats::Stats;

const GRAY: Rgb565 = Rgb565::new(16, 32, 16); // the C code's 0x8410

pub trait Screen {
    fn render(&mut self, fb: &mut FrameBuffer, stats: &mut Stats);
}

fn text_width(font: &MonoFont, text: &str) -> i32 {
    let advance = font.character_size.width + font.character_spacing;
    (text.chars().count() as u32 * advance) as i32
}

/// Fill a band of the framebuffer with black.
fn clear_band(fb: &mut FrameBuffer, y: i32, h: u32) {
    Rectangle::new(Point::new(0, y), Size::new(WIDTH as u32, h))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(fb)
        .unwrap();
}

/// Top-of-screen "ip - hostname.fqdn" line. Text wider than the display
/// scrolls as a marquee, advancing 1px per frame; shorter text sits still.
pub struct Header {
    text: String,
    offset: i32,
    span: i32, // marquee period: text width plus the gap between repeats
}

const HEADER_FONT: &MonoFont = &FONT_9X15;
const SCROLL_GAP_PX: i32 = 24;

impl Header {
    pub fn new(text: &str) -> Self {
        let span = text_width(HEADER_FONT, text) + SCROLL_GAP_PX;
        Self { text: text.to_string(), offset: 0, span }
    }

    fn scrolling(&self) -> bool {
        text_width(HEADER_FONT, &self.text) > WIDTH as i32
    }

    /// Draw the current marquee position and advance one step.
    pub fn draw(&mut self, fb: &mut FrameBuffer) {
        clear_band(fb, 0, HEADER_FONT.character_size.height);
        let style = MonoTextStyle::new(HEADER_FONT, Rgb565::WHITE);
        let mut draw_at = |x: i32| {
            Text::with_baseline(&self.text, Point::new(x, 0), style, Baseline::Top)
                .draw(fb)
                .unwrap();
        };
        if self.scrolling() {
            // Two copies, one span apart; the framebuffer clips off-screen
            // pixels, so we don't care which parts land outside.
            draw_at(-self.offset);
            draw_at(-self.offset + self.span);
            self.offset = (self.offset + 1) % self.span;
        } else {
            draw_at(0);
        }
    }
}

/// The stat line in the large font, on fixed columns so nothing shifts
/// between screens: label left-aligned at 30, value right-aligned against
/// the unit, unit fixed at 115.
fn draw_value_line(fb: &mut FrameBuffer, label: &str, value: u8, unit: &str) {
    const FONT: &MonoFont = &FONT_10X20;
    const LABEL_X: i32 = 30;
    const UNIT_X: i32 = 115;

    clear_band(fb, 35, FONT.character_size.height);
    let style = MonoTextStyle::new(FONT, Rgb565::WHITE);
    let mut draw_at = |text: &str, x: i32| {
        Text::with_baseline(text, Point::new(x, 35), style, Baseline::Top)
            .draw(fb)
            .unwrap();
    };
    let value = value.to_string();
    draw_at(label, LABEL_X);
    draw_at(&value, UNIT_X - text_width(FONT, &value));
    draw_at(unit, UNIT_X);
}

/// Ten-segment bar gauge along the bottom of the display.
fn draw_gauge(fb: &mut FrameBuffer, percent: u8, color: Rgb565) {
    let filled = (percent.min(100) as i32 + 10).min(100) / 10;
    for segment in 0..10 {
        let segment_color = if segment < filled { color } else { GRAY };
        Rectangle::new(Point::new(30 + segment * 10, 60), Size::new(6, 10))
            .into_styled(PrimitiveStyle::with_fill(segment_color))
            .draw(fb)
            .unwrap();
    }
}

pub struct CpuScreen;

impl Screen for CpuScreen {
    fn render(&mut self, fb: &mut FrameBuffer, stats: &mut Stats) {
        let cpu = stats.cpu_percent();
        draw_value_line(fb, "CPU:", cpu, "%");
        draw_gauge(fb, cpu, Rgb565::GREEN);
    }
}

pub struct RamScreen;

impl Screen for RamScreen {
    fn render(&mut self, fb: &mut FrameBuffer, stats: &mut Stats) {
        let ram = stats.memory_percent();
        draw_value_line(fb, "RAM:", ram, "%");
        draw_gauge(fb, ram, Rgb565::YELLOW);
    }
}

pub struct TempScreen;

impl Screen for TempScreen {
    fn render(&mut self, fb: &mut FrameBuffer, stats: &mut Stats) {
        let temp = stats.temperature_celsius();
        draw_value_line(fb, "TEMP:", temp, "C");
        draw_gauge(fb, temp, Rgb565::RED);
    }
}

pub struct DiskScreen;

impl Screen for DiskScreen {
    fn render(&mut self, fb: &mut FrameBuffer, stats: &mut Stats) {
        let disk = stats.disk_percent();
        draw_value_line(fb, "DISK:", disk, "%");
        draw_gauge(fb, disk, Rgb565::BLUE);
    }
}
