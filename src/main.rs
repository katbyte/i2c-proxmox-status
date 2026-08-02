//! Status display daemon for the UCTRONICS rack-mount LCD.
//!
//! Composes each frame in an in-memory framebuffer (header marquee, divider,
//! current stat screen), then differentially flushes it: only regions that
//! changed since the last flush are pushed over the slow I2C bus.

mod framebuffer;
mod screens;
mod st7735;
mod stats;

use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};

use framebuffer::FrameBuffer;
use screens::{CpuScreen, DiskScreen, Header, RamScreen, Screen, TempScreen};
use st7735::Lcd;

const SCREEN_HOLD: Duration = Duration::from_secs(2);
// Breather between frames; the marquee flush itself (~140ms on the bus) sets
// the real pace.
const FRAME_PAUSE: Duration = Duration::from_millis(20);

fn main() -> ExitCode {
    let mut lcd = match Lcd::open() {
        Ok(lcd) => lcd,
        Err(e) => {
            eprintln!("failed to open the display: {e}");
            return ExitCode::FAILURE;
        }
    };
    thread::sleep(Duration::from_secs(1));

    let mut fb = FrameBuffer::new();
    // The blue divider bar under the header; drawn once, never overdrawn.
    Rectangle::new(Point::new(0, 20), Size::new(st7735::WIDTH as u32, 5))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLUE))
        .draw(&mut fb)
        .unwrap();

    let mut header = Header::new(&format!("{} - {}", stats::ip_address(), stats::fqdn()));
    let mut stats = stats::Stats::new();

    let mut screens: [Box<dyn Screen>; 4] = [
        Box::new(CpuScreen),
        Box::new(RamScreen),
        Box::new(TempScreen),
        Box::new(DiskScreen),
    ];

    loop {
        for screen in &mut screens {
            screen.render(&mut fb, &mut stats);
            let deadline = Instant::now() + SCREEN_HOLD;
            loop {
                header.draw(&mut fb);
                if let Err(e) = fb.flush(&mut lcd) {
                    eprintln!("flush failed: {e}");
                }
                if Instant::now() >= deadline {
                    break;
                }
                thread::sleep(FRAME_PAUSE);
            }
        }
    }
}
