//! The rotating status screens, ported from the lcd_display_* functions in
//! `hardware/st7735/st7735.c`.
//!
//! Layout contract (inherited from the C code): the CPU screen repaints the
//! whole display including the IP header; the other screens only repaint the
//! middle band and the gauge, so they must run after the CPU screen has laid
//! down the header.

use std::io;

use crate::fonts::{FONT_11X18, FONT_8X16};
use crate::st7735::{self, Lcd};
use crate::stats;

pub trait Screen {
    fn render(&mut self, lcd: &mut Lcd) -> io::Result<()>;
}

/// Ten-segment bar gauge along the bottom of the display.
fn draw_gauge(lcd: &mut Lcd, percent: u8, color: u16) -> io::Result<()> {
    let filled = (percent.min(100) as u16 + 10).min(100) / 10;
    for segment in 0..10u16 {
        let segment_color = if segment < filled { color } else { st7735::GRAY };
        lcd.fill_rectangle(30 + segment * 10, 60, 6, 10, segment_color)?;
    }
    Ok(())
}

/// Clear the middle band and draw "LABEL: <value><unit>" in the large font.
fn draw_value_line(lcd: &mut Lcd, label: &str, x: u16, value: u8, unit: &str) -> io::Result<()> {
    lcd.fill_rectangle(0, 35, st7735::WIDTH, 20, st7735::BLACK)?;
    lcd.write_string(x, 35, label, &FONT_11X18, st7735::WHITE, st7735::BLACK)?;
    let text = format!("{value}{unit}");
    let value_x = x + label.len() as u16 * FONT_11X18.width;
    lcd.write_string(value_x, 35, &text, &FONT_11X18, st7735::WHITE, st7735::BLACK)
}

pub struct CpuScreen;

impl Screen for CpuScreen {
    fn render(&mut self, lcd: &mut Lcd) -> io::Result<()> {
        let cpu = stats::cpu_percent()?;
        lcd.fill_screen(st7735::BLACK)?;
        lcd.fill_rectangle(0, 20, st7735::WIDTH, 5, st7735::BLUE)?;
        lcd.write_string(0, 0, "IP:", &FONT_8X16, st7735::WHITE, st7735::BLACK)?;
        lcd.write_string(24, 0, &stats::ip_address(), &FONT_8X16, st7735::WHITE, st7735::BLACK)?;
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
