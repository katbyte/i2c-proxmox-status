//! Status display daemon for the UCTRONICS rack-mount LCD.
//!
//! Composes each frame in an in-memory framebuffer (header marquee, divider,
//! current stat screen), then differentially flushes it: only regions that
//! changed since the last flush are pushed over the slow I2C bus.
//!
//! Build with `--features simulator` to preview in an SDL window instead of
//! driving the real panel.

mod framebuffer;
mod screens;
#[cfg(feature = "simulator")]
mod sim;
mod st7735;
mod stats;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::text::{Baseline, Text};

use framebuffer::{FrameBuffer, PixelSink};
use screens::{
    status_color, text_width, CpuScreen, CpusPage, DiskPage, DiskScreen, Header, HeaderAlign,
    HeaderMode, MemPage, NetPage, Page, PageKind, PageMode, ProxmoxPage, RamScreen, TempScreen,
    TempsPage, WarningsPage, DIVIDER_H, DIVIDER_Y, GRAPH_WIDTH, IO_GRAPH_WIDTH, MEM_GRAPH_WIDTH,
    NET_GRAPH_WIDTH, PAGE_HEIGHT, PAGE_TOP, TEMP_GRAPH_WIDTH,
};

// Breather between frames; the marquee flush itself (~140ms on the bus) sets
// the real pace.
const FRAME_PAUSE: Duration = Duration::from_millis(20);
// Page wipes redraw the whole page area every frame (~18KB, ~450ms on the
// real bus), so keep them to a few big steps.
const PAGE_WIPE_H_STEP: i32 = 40;
const PAGE_WIPE_V_STEP: i32 = 14;

/// Status display daemon for the UCTRONICS rack-mount LCD.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Render to an SDL window instead of the real panel
    /// (needs a build with `--features simulator`)
    #[arg(short, long)]
    simulator: bool,

    /// How the header pages between the IP and the hostname: cut swaps
    /// instantly, the wipes animate, slide is a continuous marquee of the
    /// full text
    #[arg(long, value_enum, default_value = "cut")]
    header_mode: HeaderMode,

    /// Seconds each header page (the IP, the hostname) stays up
    #[arg(long, default_value_t = 10.0)]
    header_hold: f64,

    /// Change the header only when the page rotation changes (once
    /// --header-hold has elapsed), so the two transitions land together
    #[arg(long)]
    header_sync: bool,

    /// Minimum seconds between page content refreshes — live readings and
    /// graphs redraw at most this often, leaving the bus quiet in between
    #[arg(long, default_value_t = 1.0)]
    page_refresh: f64,

    /// Seconds each page stays up before the rotation moves on, counted
    /// from when the page is fully on the glass
    #[arg(long, default_value_t = 7.0)]
    page_hold: f64,

    /// Horizontal alignment of header text that doesn't fill the display
    #[arg(long, value_enum, default_value = "center")]
    header_align: HeaderAlign,

    /// Which form of the hostname to show in the header
    #[arg(long, value_enum, default_value = "hostname")]
    host: HostStyle,

    /// Pages to rotate through, in order (comma separated)
    #[arg(
        long,
        value_enum,
        value_delimiter = ',',
        default_value = "cpus,temps,mem,disk,network,proxmox,warnings"
    )]
    pages: Vec<PageKind>,

    /// Transition when switching pages
    #[arg(long, value_enum, default_value = "cut")]
    page_mode: PageMode,

    /// Seconds of history a page's graphs span (cpus, mem)
    #[arg(long, alias = "cpu-history", default_value_t = 120)]
    page_history: u64,

    /// Seconds of history the disk IOPS sparkline spans
    #[arg(long, default_value_t = 300)]
    io_history: u64,

    /// Seconds of history the temps graphs span; temperatures drift slowly,
    /// so they get a much longer window than the other pages
    #[arg(long, default_value_t = 3600)]
    temp_history: u64,

    /// Seconds of history the memory graph spans
    #[arg(long, default_value_t = 3600)]
    mem_history: u64,

    /// Seconds of history the network graph spans (minimum 300)
    #[arg(long, default_value_t = 300)]
    net_history: u64,

    /// Text left on the panel after a clean exit (ctrl-c / SIGTERM) — the
    /// bridge MCU keeps showing the last frame after the daemon stops
    #[arg(long, default_value = "CLOSED")]
    close_text: String,

    /// Text left on the panel if the daemon crashes
    #[arg(long, default_value = "CRASH")]
    crash_text: String,
}

#[derive(Clone, Copy, ValueEnum)]
enum HostStyle {
    /// Fully qualified name (host.example.com)
    Fqdn,
    /// Short name only
    Hostname,
}

fn main() -> ExitCode {
    let args = Args::parse();

    if args.simulator {
        #[cfg(feature = "simulator")]
        return run(sim::SimLcd::new(), &args);
        #[cfg(not(feature = "simulator"))]
        {
            eprintln!("this binary was built without the simulator; rebuild with --features simulator");
            return ExitCode::FAILURE;
        }
    }

    let lcd = match st7735::Lcd::open() {
        Ok(lcd) => lcd,
        Err(e) => {
            eprintln!("failed to open the display: {e}");
            return ExitCode::FAILURE;
        }
    };
    thread::sleep(Duration::from_secs(1));
    run(lcd, &args)
}

/// Set by SIGINT/SIGTERM; the frame loop exits cleanly at the next frame.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_shutdown_signal(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn install_shutdown_handler() {
    let handler = on_shutdown_signal as extern "C" fn(libc::c_int);
    unsafe {
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handler as libc::sighandler_t);
    }
}

/// Leave a final full-screen message on the glass; the bridge MCU keeps
/// displaying it after we're gone. Flush errors are ignored — if this is
/// the crash path the bus may be the thing that broke.
fn draw_exit_screen(sink: &mut impl PixelSink, text: &str, color: Rgb565) {
    let mut fb = FrameBuffer::new();
    let x = ((st7735::WIDTH as i32 - text_width(&FONT_10X20, text)) / 2).max(0);
    let y = (st7735::HEIGHT as i32 - FONT_10X20.character_size.height as i32) / 2;
    let style = MonoTextStyle::new(&FONT_10X20, color);
    Text::with_baseline(text, Point::new(x, y), style, Baseline::Top)
        .draw(&mut fb)
        .unwrap();
    let _ = fb.flush(sink);
}

fn run(mut sink: impl PixelSink, args: &Args) -> ExitCode {
    install_shutdown_handler();
    match catch_unwind(AssertUnwindSafe(|| main_loop(&mut sink, args))) {
        Ok(code) => {
            draw_exit_screen(&mut sink, &args.close_text, Rgb565::WHITE);
            code
        }
        Err(_) => {
            draw_exit_screen(&mut sink, &args.crash_text, Rgb565::RED);
            ExitCode::FAILURE
        }
    }
}

fn main_loop(sink: &mut impl PixelSink, args: &Args) -> ExitCode {
    let mut fb = FrameBuffer::new();
    // The blue divider bar under the header; drawn once, never overdrawn.
    Rectangle::new(Point::new(0, DIVIDER_Y), Size::new(st7735::WIDTH as u32, DIVIDER_H))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLUE))
        .draw(&mut fb)
        .unwrap();

    let host = match args.host {
        HostStyle::Fqdn => stats::fqdn(),
        HostStyle::Hostname => stats::hostname(),
    };
    let mut header = Header::new(
        &format!("{} - {}", stats::ip_address(), host),
        args.header_mode,
        args.header_align,
        Duration::from_secs_f64(args.header_hold.max(0.1)),
        args.header_sync,
    );
    // Get the header and divider on the glass right away — stats warm-up
    // and the first full page compose take noticeably longer.
    header.draw(&mut fb, false);
    if let Err(e) = fb.flush(sink) {
        eprintln!("flush failed: {e}");
    }
    let mut stats = stats::Stats::new();

    if args.pages.is_empty() {
        eprintln!("no pages selected");
        return ExitCode::FAILURE;
    }

    // The sampler thread keeps the stat history moving even while other
    // pages are showing. Each series samples once per graph column across
    // its window, so the graphs span exactly their configured seconds.
    let wants_sampler = args.pages.iter().any(|kind| {
        matches!(
            kind,
            PageKind::Cpus
                | PageKind::Temps
                | PageKind::Mem
                | PageKind::Disk
                | PageKind::Network
                | PageKind::Proxmox
        )
    });
    let sampler = wants_sampler.then(|| {
        let interval = |window_secs: u64, columns: usize| {
            Duration::from_millis((window_secs.max(1) * 1000 / columns as u64).max(100))
        };
        stats::Sampler::start(stats::SamplerConfig {
            cpu: interval(args.page_history, GRAPH_WIDTH),
            temp: interval(args.temp_history, TEMP_GRAPH_WIDTH),
            mem: interval(args.mem_history, MEM_GRAPH_WIDTH),
            io: interval(args.io_history, IO_GRAPH_WIDTH),
            net: interval(args.net_history.max(300), NET_GRAPH_WIDTH),
        })
    });

    let mut pages: Vec<Box<dyn Page>> = args
        .pages
        .iter()
        .map(|kind| -> Box<dyn Page> {
            match kind {
                PageKind::Cpus => Box::new(CpusPage::new(sampler.clone().unwrap())),
                PageKind::Temps => Box::new(TempsPage::new(sampler.clone().unwrap())),
                PageKind::Mem => Box::new(MemPage::new(sampler.clone().unwrap())),
                PageKind::Disk => Box::new(DiskPage::new(sampler.clone().unwrap())),
                PageKind::Network => Box::new(NetPage::new(sampler.clone().unwrap())),
                PageKind::Warnings => Box::new(WarningsPage),
                PageKind::Proxmox => Box::new(ProxmoxPage::new(sampler.clone().unwrap())),
                PageKind::UctronicsCpu => Box::new(CpuScreen),
                PageKind::UctronicsRam => Box::new(RamScreen),
                PageKind::UctronicsTemp => Box::new(TempScreen),
                PageKind::UctronicsDisk => Box::new(DiskScreen),
            }
        })
        .collect();

    let page_refresh = Duration::from_secs_f64(args.page_refresh.max(0.05));
    let page_hold = Duration::from_secs_f64(args.page_hold.max(0.5));
    // Start on the first page that wants to show (warnings may be clear).
    let mut current = 0;
    for _ in 0..pages.len() {
        if pages[current].active(&mut stats) {
            break;
        }
        current = (current + 1) % pages.len();
    }
    loop {
        // Hold the page, re-rendering every frame: content that hasn't
        // changed diffs to nothing, so only real updates hit the bus. The
        // hold clock starts after the first flush returns — i.e. once the
        // page is fully on the glass — so slow initial draws don't eat
        // into its display time.
        let mut deadline = None;
        let mut next_refresh: Option<Instant> = None;
        let mut refresh_deferred = false;
        loop {
            if SHUTDOWN.load(Ordering::SeqCst) {
                return ExitCode::SUCCESS;
            }
            let header_changed = header.draw(&mut fb, false);
            // Page content refreshes at most once per --page-refresh, and a
            // refresh due on the frame the header changed waits one frame,
            // so the header's band flush gets the bus to itself.
            let refresh_due = next_refresh.map_or(true, |at| Instant::now() >= at);
            if refresh_due && (!header_changed || refresh_deferred) {
                header.set_color(status_color(&mut stats));
                clear_page_area(&mut fb);
                draw_page(&mut fb, pages[current].as_mut(), &mut stats, Point::zero());
                next_refresh = Some(Instant::now() + page_refresh);
                refresh_deferred = false;
            } else if refresh_due {
                refresh_deferred = true;
            }
            if let Err(e) = fb.flush(sink) {
                eprintln!("flush failed: {e}");
            }
            let deadline = *deadline.get_or_insert_with(|| Instant::now() + page_hold);
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(FRAME_PAUSE);
        }

        // Advance to the next page that wants to show; a page can decline
        // (the warnings page while everything is healthy).
        let mut next = (current + 1) % pages.len();
        while next != current && !pages[next].active(&mut stats) {
            next = (next + 1) % pages.len();
        }
        header.trigger();
        let (dx, dy) = args.page_mode.wipe_dir();
        if next != current && (dx, dy) != (0, 0) {
            // Old page slides out in the wipe direction, the next trails in
            // one page-size behind; the final exact position is drawn by the
            // hold loop above.
            let (limit, step) = if dx != 0 {
                (st7735::WIDTH as i32, PAGE_WIPE_H_STEP)
            } else {
                (PAGE_HEIGHT as i32, PAGE_WIPE_V_STEP)
            };
            let mut progress = step;
            while progress < limit && !SHUTDOWN.load(Ordering::SeqCst) {
                // busy: the wipe already redraws the whole page area every
                // frame, so hold any header page swap until it's done.
                header.draw(&mut fb, true);
                clear_page_area(&mut fb);
                let out = Point::new(dx * progress, dy * progress);
                let inc = Point::new(dx * (progress - limit), dy * (progress - limit));
                draw_page(&mut fb, pages[current].as_mut(), &mut stats, out);
                draw_page(&mut fb, pages[next].as_mut(), &mut stats, inc);
                if let Err(e) = fb.flush(sink) {
                    eprintln!("flush failed: {e}");
                }
                thread::sleep(FRAME_PAUSE);
                progress += step;
            }
        }
        current = next;
    }
}

fn page_area() -> Rectangle {
    Rectangle::new(Point::new(0, PAGE_TOP), Size::new(st7735::WIDTH as u32, PAGE_HEIGHT))
}

fn clear_page_area(fb: &mut FrameBuffer) {
    page_area()
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(fb)
        .unwrap();
}

/// Render a page clipped to the page area, shifted by `offset` (zero when
/// static, the animation offset mid-wipe).
fn draw_page(fb: &mut FrameBuffer, page: &mut dyn Page, stats: &mut stats::Stats, offset: Point) {
    let area = page_area();
    let mut clipped = fb.clipped(&area);
    let mut target = clipped.translated(Point::new(offset.x, PAGE_TOP + offset.y));
    page.render(&mut target, stats);
}
