//! Status display daemon for the UCTRONICS rack-mount LCD.
//!
//! Paints the static background once, then cycles the four status screens
//! (one every two seconds) while the header marquee scrolls continuously.

mod fonts;
mod screens;
mod st7735;
mod stats;

use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use screens::{CpuScreen, DiskScreen, Header, RamScreen, Screen, TempScreen};
use st7735::Lcd;

const SCREEN_HOLD: Duration = Duration::from_secs(2);
// The header blit itself takes ~140ms on the I2C bus; this small pause just
// keeps the loop from spinning when the header isn't scrolling.
const HEADER_TICK: Duration = Duration::from_millis(15);

fn main() -> ExitCode {
    let mut lcd = match Lcd::open() {
        Ok(lcd) => lcd,
        Err(e) => {
            eprintln!("failed to open the display: {e}");
            return ExitCode::FAILURE;
        }
    };
    thread::sleep(Duration::from_secs(1));

    let mut header = Header::new(&format!("{} - {}", stats::ip_address(), stats::fqdn()));

    // Static background: black everywhere, blue divider under the header.
    // Screens and header only repaint their own regions after this.
    if let Err(e) = lcd
        .fill_screen(st7735::BLACK)
        .and_then(|_| lcd.fill_rectangle(0, 20, st7735::WIDTH, 5, st7735::BLUE))
        .and_then(|_| header.render(&mut lcd))
    {
        eprintln!("failed to draw the background: {e}");
        return ExitCode::FAILURE;
    }

    let mut screens: [Box<dyn Screen>; 4] = [
        Box::new(CpuScreen::new()),
        Box::new(RamScreen),
        Box::new(TempScreen),
        Box::new(DiskScreen),
    ];

    loop {
        for screen in &mut screens {
            if let Err(e) = screen.render(&mut lcd) {
                eprintln!("render failed: {e}");
            }
            let deadline = Instant::now() + SCREEN_HOLD;
            while Instant::now() < deadline {
                if let Err(e) = header.tick(&mut lcd) {
                    eprintln!("header render failed: {e}");
                }
                thread::sleep(HEADER_TICK);
            }
        }
    }
}
