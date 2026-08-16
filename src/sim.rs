//! SDL-window stand-in for the real panel (`cargo run --features simulator`).
//!
//! Renders into an embedded-graphics simulator window, but sleeps through a
//! timing model of the real I2C path first, so the marquee cadence and the
//! screen-switch hitches look like they will on the rack. The model mirrors
//! `Lcd::blit`: 400kHz bus at ~9 bits/byte, 700µs pause per 160-byte burst
//! chunk, and 7 register commands per ≤16-row session.

use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use embedded_graphics::pixelcolor::raw::RawU16;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics_simulator::{
    OutputSettingsBuilder, SimulatorDisplay, SimulatorEvent, Window,
};

use crate::framebuffer::PixelSink;
use crate::st7735::{HEIGHT, WIDTH};

pub struct SimLcd {
    display: SimulatorDisplay<Rgb565>,
    window: Window,
    // With SIM_SNAPSHOT_DIR set, a PNG of the panel lands there once a
    // second (frame_0000.png, ...) so layouts can be reviewed offline.
    snapshots: Option<PathBuf>,
    last_snapshot: Option<Instant>,
    snapshot_count: u32,
}

impl SimLcd {
    pub fn new() -> Self {
        Self {
            display: SimulatorDisplay::new(Size::new(WIDTH as u32, HEIGHT as u32)),
            window: Window::new(
                "i2c-proxmox-status (simulated panel)",
                &OutputSettingsBuilder::new().scale(4).build(),
            ),
            snapshots: std::env::var_os("SIM_SNAPSHOT_DIR").map(PathBuf::from),
            last_snapshot: None,
            snapshot_count: 0,
        }
    }

    fn snapshot(&mut self) {
        let Some(dir) = &self.snapshots else { return };
        if self
            .last_snapshot
            .is_some_and(|last| last.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        self.last_snapshot = Some(Instant::now());
        let path = dir.join(format!("frame_{:04}.png", self.snapshot_count));
        self.snapshot_count += 1;
        let image = self
            .display
            .to_rgb_output_image(&OutputSettingsBuilder::new().build());
        if let Err(e) = image.save_png(&path) {
            eprintln!("snapshot failed: {e}");
        }
    }
}

impl PixelSink for SimLcd {
    fn blit(&mut self, x: u16, y: u16, w: u16, h: u16, pixels: &[u16]) -> io::Result<()> {
        thread::sleep(bus_time(w, h));

        let pixels = pixels[..w as usize * h as usize]
            .iter()
            .enumerate()
            .map(|(i, &raw)| {
                let point = Point::new(
                    x as i32 + (i % w as usize) as i32,
                    y as i32 + (i / w as usize) as i32,
                );
                Pixel(point, Rgb565::from(RawU16::new(raw)))
            });
        self.display.draw_iter(pixels).unwrap();

        self.window.update(&self.display);
        self.snapshot();
        if self
            .window
            .events()
            .any(|e| matches!(e, SimulatorEvent::Quit))
        {
            std::process::exit(0);
        }
        Ok(())
    }
}

/// How long the real bridge would take to accept this blit.
fn bus_time(w: u16, h: u16) -> Duration {
    const BIT: f64 = 1.0 / 400_000.0; // 400kHz bus
    const BYTE: f64 = 9.0 * BIT; // 8 data bits + ACK
    const CHUNK_PAUSE: f64 = 700e-6; // settle delay per 160-byte chunk
    const COMMAND: f64 = 4.0 * BYTE + 10e-6; // address + 3 bytes + settle
    const COMMANDS_PER_SESSION: f64 = 7.0; // window setup, burst on/off, syncs

    let bytes = 2.0 * w as f64 * h as f64;
    let chunks = (bytes / 160.0).ceil();
    let session_rows = (16 * 160 / (2 * w as usize)).max(1);
    let sessions = (h as f64 / session_rows as f64).ceil();

    let secs = bytes * BYTE
        + chunks * (BYTE + CHUNK_PAUSE) // chunk address byte + pause
        + sessions * COMMANDS_PER_SESSION * COMMAND;
    Duration::from_secs_f64(secs)
}
