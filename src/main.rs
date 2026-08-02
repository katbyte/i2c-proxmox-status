//! Rust port of the UCTRONICS SKU_RM0004 display daemon (`project/display.c`).
//!
//! Cycles through the status screens on the front-panel LCD, one every two
//! seconds, matching the original C behavior.

mod fonts;
mod screens;
mod st7735;
mod stats;

use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use screens::{CpuScreen, DiskScreen, RamScreen, Screen, TempScreen};
use st7735::Lcd;

fn main() -> ExitCode {
    let mut lcd = match Lcd::open() {
        Ok(lcd) => lcd,
        Err(e) => {
            eprintln!("failed to open the display: {e}");
            return ExitCode::FAILURE;
        }
    };
    thread::sleep(Duration::from_secs(1));

    let mut screens: [Box<dyn Screen>; 4] = [
        Box::new(CpuScreen),
        Box::new(RamScreen),
        Box::new(TempScreen),
        Box::new(DiskScreen),
    ];

    loop {
        for screen in &mut screens {
            if let Err(e) = screen.render(&mut lcd) {
                eprintln!("render failed: {e}");
            }
            thread::sleep(Duration::from_secs(2));
        }
    }
}
