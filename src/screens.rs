//! The header marquee and the rotating pages, drawn with embedded-graphics
//! primitives into the framebuffer. Nothing here touches the display — main
//! flushes the framebuffer after composing a frame.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::{Duration, Instant};

use clap::ValueEnum;
use embedded_graphics::draw_target::{Clipped, Translated};
use embedded_graphics::mono_font::ascii::{FONT_4X6, FONT_6X10, FONT_9X15};
use embedded_graphics::mono_font::iso_8859_1::FONT_10X20; // iso variant for the ° glyph
use embedded_graphics::mono_font::{MonoFont, MonoTextStyle};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::text::{Baseline, Text};

use crate::framebuffer::FrameBuffer;
use crate::st7735::{HEIGHT, WIDTH};
use crate::stats::{self, Sampler, Stats};

const GRAY: Rgb565 = Rgb565::new(16, 32, 16); // the C code's 0x8410
const ORANGE: Rgb565 = Rgb565::new(31, 30, 0);

/// The divider bar between the header band (15px of FONT_9X15) and the page
/// area, and where the page area starts below it.
pub const DIVIDER_Y: i32 = 16;
pub const DIVIDER_H: u32 = 2;
pub const PAGE_TOP: i32 = DIVIDER_Y + DIVIDER_H as i32 + 1;
pub const PAGE_HEIGHT: u32 = HEIGHT as u32 - PAGE_TOP as u32;

/// Drawing surface handed to a page: the framebuffer clipped to the page
/// area under the divider and translated so pages draw from (0, 0). During
/// a page wipe the translation also carries the animation offset.
pub type PageTarget<'a, 'b> = Translated<'a, Clipped<'b, FrameBuffer>>;

/// One selectable display page. A page renders its whole area from current
/// data every frame; the differential flush means an unchanged page costs
/// nothing on the bus, so content should be treated as static while shown.
pub trait Page {
    fn render(&mut self, fb: &mut PageTarget<'_, '_>, stats: &mut Stats);

    /// Whether the rotation should show this page right now. Almost every
    /// page always shows; the warnings page hides itself when clear.
    fn active(&mut self, _stats: &mut Stats) -> bool {
        true
    }
}

/// Pages available for the rotation (`--pages`). The uctronics-* pages are
/// the single-stat screens carried over from the original C firmware.
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum PageKind {
    /// btop-style per-core CPU usage with history sparklines
    Cpus,
    /// CPU and NVMe temperatures: history sparkline + big current reading
    Temps,
    /// Memory usage: history sparkline + big current percent
    Mem,
    /// Root filesystem usage plus a disk IOPS sparkline
    Disk,
    /// Network in/out rate sparklines with current rates
    Network,
    /// Host problems (throttling, heat, full disk); hidden while clear
    Warnings,
    /// Proxmox node: running/defined VM count, uptime, load history graph
    Proxmox,
    /// Overall CPU percent with a segment gauge
    UctronicsCpu,
    /// RAM percent with a segment gauge
    UctronicsRam,
    /// SoC temperature with a segment gauge
    UctronicsTemp,
    /// Root filesystem percent with a segment gauge
    UctronicsDisk,
}

/// How the display transitions between pages.
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum PageMode {
    /// Instant switch
    Cut,
    /// Next page slides in from the bottom
    WipeUp,
    /// Next page slides in from the top
    WipeDown,
    /// Next page slides in from the right
    WipeLeft,
    /// Next page slides in from the left
    WipeRight,
}

impl PageMode {
    /// Unit vector of page motion during the transition; (0, 0) for cut.
    pub fn wipe_dir(self) -> (i32, i32) {
        match self {
            PageMode::Cut => (0, 0),
            PageMode::WipeUp => (0, -1),
            PageMode::WipeDown => (0, 1),
            PageMode::WipeLeft => (-1, 0),
            PageMode::WipeRight => (1, 0),
        }
    }
}

pub fn text_width(font: &MonoFont, text: &str) -> i32 {
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

/// How the header shows text that doesn't fit (or pages between the IP and
/// the hostname).
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum HeaderMode {
    /// Page by page; instant swap, no animation
    Cut,
    /// Continuous marquee, advancing 1px per frame
    Slide,
    /// Page by page; the next page slides in from the bottom
    WipeUp,
    /// Page by page; the next page slides in from the top
    WipeDown,
    /// Page by page; the next page slides in from the right
    WipeLeft,
    /// Page by page; the next page slides in from the left
    WipeRight,
}

impl HeaderMode {
    /// Unit vector of page motion during a wipe.
    fn wipe_dir(self) -> (i32, i32) {
        match self {
            HeaderMode::Cut | HeaderMode::Slide => (0, 0),
            HeaderMode::WipeUp => (0, -1),
            HeaderMode::WipeDown => (0, 1),
            HeaderMode::WipeLeft => (-1, 0),
            HeaderMode::WipeRight => (1, 0),
        }
    }
}

/// Where header text sits horizontally when it doesn't fill the display.
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum HeaderAlign {
    Left,
    #[value(alias = "centre")]
    Center,
    Right,
}

/// Top-of-screen "ip - hostname" line. Text wider than the display animates
/// per the selected [`HeaderMode`]; shorter text sits still.
pub struct Header {
    text: String,
    mode: HeaderMode,
    align: HeaderAlign,
    hold: Duration, // how long each page shows in the cut/wipe modes
    color: Rgb565,  // text color; the caller sets it to reflect host status
    sync: bool,     // change only at page-rotation changes (see trigger())
    armed: bool,    // sync mode: a rotation change just happened
    // slide state
    offset: i32,
    span: i32, // marquee period: text width plus the gap between repeats
    // cut/wipe state
    pages: Vec<String>,
    page: usize,
    phase: Phase,
}

#[derive(Clone, Copy)]
enum Phase {
    Hold(Instant), // showing a page until this deadline
    Wipe(i32),     // mid-transition, progress in pixels
}

const HEADER_FONT: &MonoFont = &FONT_9X15;
const SCROLL_GAP_PX: i32 = 24;
// Wipe speeds in px per frame; a frame is ~150ms on the real bus while the
// header band is changing. Horizontal wipes travel the full 160px width,
// vertical ones only the 15px band height, hence the different steps.
const WIPE_H_STEP: i32 = 16;
const WIPE_V_STEP: i32 = 3;

fn draw_header_text<D>(target: &mut D, text: &str, origin: Point, color: Rgb565)
where
    D: DrawTarget<Color = Rgb565, Error = Infallible>,
{
    let style = MonoTextStyle::new(HEADER_FONT, color);
    Text::with_baseline(text, origin, style, Baseline::Top)
        .draw(target)
        .unwrap();
}

/// Header status color: white while healthy, escalating yellow / orange /
/// red as the hottest sensor or the root filesystem heads for trouble.
pub fn status_color(stats: &mut Stats) -> Rgb565 {
    let (cpu, nvme) = stats.temps_now();
    let temp = cpu.unwrap_or(0).max(nvme.unwrap_or(0));
    let temp_level = match temp {
        0..=64 => 0,
        65..=74 => 1,
        75..=84 => 2,
        _ => 3,
    };
    let disk_level = match stats.disk_percent() {
        0..=79 => 0,
        80..=89 => 1,
        90..=94 => 2,
        _ => 3,
    };
    // Pi firmware flags: active throttling is red, since-boot is a caution.
    let flags = stats::throttled_flags();
    let throttle_level = if flags & 0xf != 0 {
        3
    } else if flags & 0xf0000 != 0 {
        1
    } else {
        0
    };
    match temp_level.max(disk_level).max(throttle_level) {
        0 => Rgb565::WHITE,
        1 => Rgb565::YELLOW,
        2 => ORANGE,
        _ => Rgb565::RED,
    }
}

/// Split text into pages that each fit the display width, for the wipe modes.
fn paginate(text: &str, font: &MonoFont) -> Vec<String> {
    let advance = (font.character_size.width + font.character_spacing) as usize;
    let per_page = (WIDTH as usize / advance).max(1);
    text.chars()
        .collect::<Vec<_>>()
        .chunks(per_page)
        .map(|c| c.iter().collect::<String>().trim().to_string())
        .collect()
}

impl Header {
    pub fn new(
        text: &str,
        mode: HeaderMode,
        align: HeaderAlign,
        hold: Duration,
        sync: bool,
    ) -> Self {
        let span = text_width(HEADER_FONT, text) + SCROLL_GAP_PX;
        Self {
            text: text.to_string(),
            mode,
            align,
            hold,
            color: Rgb565::WHITE,
            sync,
            armed: false,
            offset: 0,
            span,
            // Cut/wipe modes page at the " - " between the IP and the
            // hostname; a part still too wide is further split to fit.
            pages: text
                .split(" - ")
                .flat_map(|part| paginate(part, HEADER_FONT))
                .collect(),
            page: 0,
            phase: Phase::Hold(Instant::now() + hold),
        }
    }

    /// Set the header text color (the host status from [`status_color`]).
    pub fn set_color(&mut self, color: Rgb565) {
        self.color = color;
    }

    /// The page rotation is changing now. In sync mode this is the only
    /// moment the header may change too (hold permitting), so both
    /// transitions land together. One-shot: consumed by the next draw.
    pub fn trigger(&mut self) {
        self.armed = true;
    }

    /// Whether this draw may advance to the next header page. Sync mode
    /// waits for a rotation change (and deliberately ignores `busy` then,
    /// so the header animates in step with the page transition).
    fn may_advance(&mut self, busy: bool) -> bool {
        let due = matches!(self.phase, Phase::Hold(deadline) if Instant::now() >= deadline);
        if self.sync {
            let go = self.armed && due;
            self.armed = false;
            go
        } else {
            !busy && due
        }
    }

    fn scrolling(&self) -> bool {
        text_width(HEADER_FONT, &self.text) > WIDTH as i32
    }

    /// X origin that places `text` per the alignment flag.
    fn align_x(&self, text: &str) -> i32 {
        let slack = WIDTH as i32 - text_width(HEADER_FONT, text);
        match self.align {
            HeaderAlign::Left => 0,
            HeaderAlign::Center => (slack / 2).max(0),
            HeaderAlign::Right => slack.max(0),
        }
    }

    /// Draw the current frame and advance one step. `busy` means the page
    /// area is mid-transition and hogging the bus — page swaps are deferred
    /// until it clears so a header flush doesn't pile on. Returns whether
    /// the band's content changed this frame, so the caller can give the
    /// header's flush the bus to itself.
    pub fn draw(&mut self, fb: &mut FrameBuffer, busy: bool) -> bool {
        clear_band(fb, 0, HEADER_FONT.character_size.height);
        match self.mode {
            HeaderMode::Slide => self.draw_slide(fb),
            _ if self.pages.len() <= 1 => {
                draw_header_text(
                    fb,
                    &self.text,
                    Point::new(self.align_x(&self.text), 0),
                    self.color,
                );
                false
            }
            HeaderMode::Cut => self.draw_cut(fb, busy),
            _ => self.draw_wipe(fb, busy),
        }
    }

    /// Instant page swap once the hold expires — no animation, so the only
    /// bus cost is the single band flush of the new text.
    fn draw_cut(&mut self, fb: &mut FrameBuffer, busy: bool) -> bool {
        let changed = self.may_advance(busy);
        if changed {
            self.page = (self.page + 1) % self.pages.len();
            self.phase = Phase::Hold(Instant::now() + self.hold);
        }
        let page = &self.pages[self.page];
        draw_header_text(fb, page, Point::new(self.align_x(page), 0), self.color);
        changed
    }

    fn draw_slide(&mut self, fb: &mut FrameBuffer) -> bool {
        if self.scrolling() {
            // Two copies, one span apart; the framebuffer clips off-screen
            // pixels, so we don't care which parts land outside.
            draw_header_text(fb, &self.text, Point::new(-self.offset, 0), self.color);
            draw_header_text(
                fb,
                &self.text,
                Point::new(-self.offset + self.span, 0),
                self.color,
            );
            self.offset = (self.offset + 1) % self.span;
            true
        } else {
            draw_header_text(
                fb,
                &self.text,
                Point::new(self.align_x(&self.text), 0),
                self.color,
            );
            false
        }
    }

    fn draw_wipe(&mut self, fb: &mut FrameBuffer, busy: bool) -> bool {
        let h = HEADER_FONT.character_size.height;
        match self.phase {
            Phase::Hold(_) => {
                let start = self.may_advance(busy);
                let page = &self.pages[self.page];
                draw_header_text(fb, page, Point::new(self.align_x(page), 0), self.color);
                if start {
                    self.phase = Phase::Wipe(0);
                }
                false
            }
            Phase::Wipe(progress) => {
                let next = (self.page + 1) % self.pages.len();
                let (dx, dy) = self.mode.wipe_dir();
                let (limit, step) = if dx != 0 {
                    (WIDTH as i32, WIPE_H_STEP)
                } else {
                    (h as i32, WIPE_V_STEP)
                };
                // Old page slides out in the wipe direction while the new one
                // trails in one screen behind it; clip so neither leaks past
                // the band into the divider.
                let old = &self.pages[self.page];
                let new = &self.pages[next];
                let old_origin = Point::new(self.align_x(old) + dx * progress, dy * progress);
                let new_origin = Point::new(
                    self.align_x(new) + dx * (progress - limit),
                    dy * (progress - limit),
                );
                let band = Rectangle::new(Point::zero(), Size::new(WIDTH as u32, h));
                let mut band_fb = fb.clipped(&band);
                draw_header_text(&mut band_fb, old, old_origin, self.color);
                draw_header_text(&mut band_fb, new, new_origin, self.color);

                let progress = progress + step;
                self.phase = if progress >= limit {
                    self.page = next;
                    Phase::Hold(Instant::now() + self.hold)
                } else {
                    Phase::Wipe(progress)
                };
                true
            }
        }
    }
}

/// The stat line in the large font, on fixed columns so nothing shifts
/// between pages: label left-aligned at 30, value right-aligned against
/// the unit, unit fixed at 115. Y is page-local; main clears the page area
/// before every render.
fn draw_value_line(fb: &mut PageTarget<'_, '_>, label: &str, value: u8, unit: &str) {
    const FONT: &MonoFont = &FONT_10X20;
    const LABEL_X: i32 = 30;
    const UNIT_X: i32 = 115;

    let style = MonoTextStyle::new(FONT, Rgb565::WHITE);
    let mut draw_at = |text: &str, x: i32| {
        Text::with_baseline(text, Point::new(x, 10), style, Baseline::Top)
            .draw(fb)
            .unwrap();
    };
    let value = value.to_string();
    draw_at(label, LABEL_X);
    draw_at(&value, UNIT_X - text_width(FONT, &value));
    draw_at(unit, UNIT_X);
}

/// Ten-segment bar gauge along the bottom of the page.
fn draw_gauge(fb: &mut PageTarget<'_, '_>, percent: u8, color: Rgb565) {
    let filled = (percent.min(100) as i32 + 10).min(100) / 10;
    for segment in 0..10 {
        let segment_color = if segment < filled { color } else { GRAY };
        Rectangle::new(Point::new(30 + segment * 10, 35), Size::new(6, 10))
            .into_styled(PrimitiveStyle::with_fill(segment_color))
            .draw(fb)
            .unwrap();
    }
}

pub struct CpuScreen;

impl Page for CpuScreen {
    fn render(&mut self, fb: &mut PageTarget<'_, '_>, stats: &mut Stats) {
        let cpu = stats.cpu_percent();
        draw_value_line(fb, "CPU:", cpu, "%");
        draw_gauge(fb, cpu, Rgb565::GREEN);
    }
}

pub struct RamScreen;

impl Page for RamScreen {
    fn render(&mut self, fb: &mut PageTarget<'_, '_>, stats: &mut Stats) {
        let ram = stats.memory_percent();
        draw_value_line(fb, "RAM:", ram, "%");
        draw_gauge(fb, ram, Rgb565::YELLOW);
    }
}

pub struct TempScreen;

impl Page for TempScreen {
    fn render(&mut self, fb: &mut PageTarget<'_, '_>, stats: &mut Stats) {
        let temp = stats.temperature_celsius();
        draw_value_line(fb, "TEMP:", temp, "C");
        draw_gauge(fb, temp, Rgb565::RED);
    }
}

pub struct DiskScreen;

impl Page for DiskScreen {
    fn render(&mut self, fb: &mut PageTarget<'_, '_>, stats: &mut Stats) {
        let disk = stats.disk_percent();
        draw_value_line(fb, "DISK:", disk, "%");
        draw_gauge(fb, disk, Rgb565::BLUE);
    }
}

// ---- the btop-style per-core CPU page ----

const CPUS_FONT: &MonoFont = &FONT_6X10;
const ROW_H: i32 = 12; // 10px font + 2px gap; 5 rows fill the 61px page
const GRAPH_X: i32 = 15; // after the two-char row labels (C*, C0..)
/// Sparkline width in px — one history sample per column. The sampler's
/// interval is derived from this so `--page-history` seconds span the graph.
pub const GRAPH_WIDTH: usize = 118;
const CORE_ROWS: usize = 4;

/// btop's default gradient: green at idle, through yellow at 50%, to red
/// when pinned — interpolated smoothly, computed per sample.
fn usage_color(percent: u8) -> Rgb565 {
    let p = percent.min(100) as u16;
    if p <= 50 {
        // green -> yellow: ramp red up
        Rgb565::new((31 * p / 50) as u8, 63, 0)
    } else {
        // yellow -> red: ramp green down
        Rgb565::new(31, (63 * (100 - p) / 50) as u8, 0)
    }
}

fn draw_cpus_percent(fb: &mut PageTarget<'_, '_>, y: i32, percent: u8) {
    let text = format!("{percent}%");
    let x = WIDTH as i32 - text_width(CPUS_FONT, &text);
    let style = MonoTextStyle::new(CPUS_FONT, usage_color(percent));
    Text::with_baseline(&text, Point::new(x, y), style, Baseline::Top)
        .draw(fb)
        .unwrap();
}

/// Per-sample columns, newest at the right edge, growing up from the
/// graph's baseline like btop's core graphs. Values are 0-100 scaled to
/// `height`; `color` maps each sample to its column color.
fn draw_sparkline(
    fb: &mut PageTarget<'_, '_>,
    samples: &VecDeque<u8>,
    x0: i32,
    y0: i32,
    width: usize,
    height: i32,
    color: impl Fn(u8) -> Rgb565,
) {
    for (i, &value) in samples.iter().rev().take(width).enumerate() {
        let x = x0 + width as i32 - 1 - i as i32;
        let bar = (value.min(100) as i32 * height + 99) / 100; // ceil: nonzero always visible
        if bar > 0 {
            Rectangle::new(Point::new(x, y0 + height - bar), Size::new(1, bar as u32))
                .into_styled(PrimitiveStyle::with_fill(color(value)))
                .draw(fb)
                .unwrap();
        }
    }
}

/// Per-core CPU page: total-CPU sparkline on top, then a usage sparkline
/// and current percent for each of the first four cores (a Pi has exactly
/// four). All rows share the same btop-style history graph and gradient.
pub struct CpusPage {
    sampler: Sampler,
}

impl CpusPage {
    pub fn new(sampler: Sampler) -> Self {
        Self { sampler }
    }
}

impl Page for CpusPage {
    fn render(&mut self, fb: &mut PageTarget<'_, '_>, _stats: &mut Stats) {
        let history = self.sampler.snapshot();
        let label_style = MonoTextStyle::new(CPUS_FONT, Rgb565::WHITE);
        let label_at = |text: &str, y: i32, fb: &mut PageTarget<'_, '_>| {
            Text::with_baseline(text, Point::new(0, y), label_style, Baseline::Top)
                .draw(fb)
                .unwrap();
        };

        let total = history.total.back().copied().unwrap_or(0);
        label_at("C*", 0, fb);
        draw_sparkline(fb, &history.total, GRAPH_X, 0, GRAPH_WIDTH, 10, usage_color);
        draw_cpus_percent(fb, 0, total);

        for (i, core) in history.cores.iter().take(CORE_ROWS).enumerate() {
            let y = (i as i32 + 1) * ROW_H;
            label_at(&format!("C{i}"), y, fb);
            draw_sparkline(fb, core, GRAPH_X, y, GRAPH_WIDTH, 10, usage_color);
            draw_cpus_percent(fb, y, core.back().copied().unwrap_or(0));
        }
    }
}

// ---- the temperatures page ----

// The big current reading ("100°C" is 50px of 10x20) leaves room for a
// narrower graph than the cpus page.
const TEMP_GRAPH_X: i32 = 26; // after the four-char labels (NVME)
/// Temps sparkline width; with `--temp-history` this sets the sample rate.
pub const TEMP_GRAPH_WIDTH: usize = 82;
const TEMP_ROW_H: i32 = 30; // two rows fill the page

/// Status color for a temperature: cool green, warm yellow, hot orange,
/// too-hot red.
fn temp_color(celsius: u8) -> Rgb565 {
    match celsius {
        0..=49 => Rgb565::GREEN,
        50..=64 => Rgb565::YELLOW,
        65..=74 => Rgb565::new(31, 30, 0), // orange
        _ => Rgb565::RED,
    }
}

/// CPU and NVMe temperatures: one row per sensor with the same btop-style
/// history sparkline, but the emphasis on a big, readable current reading
/// colored by status.
pub struct TempsPage {
    sampler: Sampler,
}

impl TempsPage {
    pub fn new(sampler: Sampler) -> Self {
        Self { sampler }
    }
}

impl Page for TempsPage {
    fn render(&mut self, fb: &mut PageTarget<'_, '_>, stats: &mut Stats) {
        let history = self.sampler.snapshot();
        let (cpu_now, nvme_now) = stats.temps_now();
        if history.temps.is_empty() {
            let style = MonoTextStyle::new(CPUS_FONT, Rgb565::WHITE);
            Text::with_baseline("no temp sensors", Point::new(0, 10), style, Baseline::Top)
                .draw(fb)
                .unwrap();
            return;
        }
        for (i, series) in history.temps.iter().take(2).enumerate() {
            let y = i as i32 * TEMP_ROW_H;
            let live = if series.label == "CPU" {
                cpu_now
            } else {
                nvme_now
            };
            let current = live.or_else(|| series.samples.back().copied()).unwrap_or(0);
            let label_style = MonoTextStyle::new(CPUS_FONT, Rgb565::WHITE);
            Text::with_baseline(
                series.label,
                Point::new(0, y + 10),
                label_style,
                Baseline::Top,
            )
            .draw(fb)
            .unwrap();
            draw_sparkline(
                fb,
                &series.samples,
                TEMP_GRAPH_X,
                y + 3,
                TEMP_GRAPH_WIDTH,
                24,
                temp_color,
            );
            // The headline reading, in the large font and status color.
            // Just the degree sign — it's always celsius.
            let text = format!("{current}\u{b0}");
            let x = WIDTH as i32 - text_width(&FONT_10X20, &text);
            let style = MonoTextStyle::new(&FONT_10X20, temp_color(current));
            Text::with_baseline(&text, Point::new(x, y + 5), style, Baseline::Top)
                .draw(fb)
                .unwrap();
        }
    }
}

// ---- the memory page ----

const MEM_GRAPH_X: i32 = 12; // after the vertical "RAM" label
/// Memory sparkline width; spans `--page-history` seconds.
pub const MEM_GRAPH_WIDTH: usize = 105;

/// One letter per row down the left edge, spread over the page height —
/// a letter-wide label column instead of a word-wide one, leaving the
/// rest of the row to the graphs. Pick a font whose letters roughly fill
/// the height so the stack reads as a word, not scattered letters.
fn draw_vertical_label(fb: &mut PageTarget<'_, '_>, text: &str, font: &MonoFont) {
    let style = MonoTextStyle::new(font, Rgb565::WHITE);
    let letters = text.chars().count() as i32;
    let letter_h = font.character_size.height as i32;
    let gap = (PAGE_HEIGHT as i32 - letters * letter_h) / (letters - 1).max(1);
    for (i, ch) in text.chars().enumerate() {
        let y = i as i32 * (letter_h + gap);
        Text::with_baseline(&ch.to_string(), Point::new(0, y), style, Baseline::Top)
            .draw(fb)
            .unwrap();
    }
}

/// Bytes as gibibytes sized for the panel: "7.6" under ten, "16" above.
fn format_gib(bytes: u64) -> String {
    let gib = bytes as f64 / (1u64 << 30) as f64;
    if gib < 10.0 {
        format!("{gib:.1}")
    } else {
        format!("{gib:.0}")
    }
}

/// Memory usage: one full-height sparkline with the big current percent and
/// the used/total figure on the right.
pub struct MemPage {
    sampler: Sampler,
}

impl MemPage {
    pub fn new(sampler: Sampler) -> Self {
        Self { sampler }
    }
}

impl Page for MemPage {
    fn render(&mut self, fb: &mut PageTarget<'_, '_>, stats: &mut Stats) {
        let history = self.sampler.snapshot();
        let (used, total) = stats.memory_used_total();
        let percent = (used * 100).checked_div(total).unwrap_or(0) as u8;

        draw_vertical_label(fb, "RAM", &FONT_10X20);
        draw_sparkline(
            fb,
            &history.mem,
            MEM_GRAPH_X,
            4,
            MEM_GRAPH_WIDTH,
            52,
            usage_color,
        );

        let text = format!("{percent}%");
        let x = WIDTH as i32 - text_width(&FONT_10X20, &text);
        let style = MonoTextStyle::new(&FONT_10X20, usage_color(percent));
        Text::with_baseline(&text, Point::new(x, 8), style, Baseline::Top)
            .draw(fb)
            .unwrap();

        // used-over-total on two lines under the percent — the total dimmed
        // so the used figure carries the row — with a vertical dimmed "GB"
        // as the shared unit column at the right edge.
        let dim = Rgb565::new(20, 40, 20);
        let num_right = WIDTH as i32 - 8;
        for (line, y, color) in [
            (format_gib(used), 30, Rgb565::WHITE),
            (format_gib(total), 45, dim),
        ] {
            let style = MonoTextStyle::new(&FONT_9X15, color);
            let x = num_right - text_width(&FONT_9X15, &line);
            Text::with_baseline(&line, Point::new(x, y), style, Baseline::Top)
                .draw(fb)
                .unwrap();
        }
        let unit = MonoTextStyle::new(CPUS_FONT, GRAY);
        for (ch, y) in [("G", 32), ("B", 44)] {
            Text::with_baseline(ch, Point::new(WIDTH as i32 - 6, y), unit, Baseline::Top)
                .draw(fb)
                .unwrap();
        }
    }
}

// ---- the disk page ----

const IO_GRAPH_X: i32 = 11; // after the vertical "DISK" label
/// IOPS sparkline width; spans `--io-history` seconds.
pub const IO_GRAPH_WIDTH: usize = 122;
const IO_COLOR: Rgb565 = Rgb565::new(10, 50, 31); // light blue

/// Tiny off-screen 1-bit buffer for scaling text past the largest bundled
/// font: render at native size, then paint each lit pixel as a
/// scale×scale block.
struct GlyphBuffer {
    w: usize,
    h: usize,
    bits: Vec<bool>,
}

impl OriginDimensions for GlyphBuffer {
    fn size(&self) -> Size {
        Size::new(self.w as u32, self.h as u32)
    }
}

impl DrawTarget for GlyphBuffer {
    type Color = Rgb565;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, color) in pixels {
            if (0..self.w as i32).contains(&point.x) && (0..self.h as i32).contains(&point.y) {
                self.bits[point.y as usize * self.w + point.x as usize] = color != Rgb565::BLACK;
            }
        }
        Ok(())
    }
}

/// Draw `text` at an integer multiple of the font's native size.
fn draw_scaled_text(
    fb: &mut PageTarget<'_, '_>,
    text: &str,
    font: &MonoFont,
    origin: Point,
    scale: i32,
    color: Rgb565,
) {
    let w = text_width(font, text) as usize;
    let h = font.character_size.height as usize;
    let mut buf = GlyphBuffer {
        w,
        h,
        bits: vec![false; w * h],
    };
    let style = MonoTextStyle::new(font, Rgb565::WHITE);
    Text::with_baseline(text, Point::zero(), style, Baseline::Top)
        .draw(&mut buf)
        .unwrap();
    for y in 0..h {
        for x in 0..w {
            if buf.bits[y * w + x] {
                let at = Point::new(origin.x + x as i32 * scale, origin.y + y as i32 * scale);
                Rectangle::new(at, Size::new(scale as u32, scale as u32))
                    .into_styled(PrimitiveStyle::with_fill(color))
                    .draw(fb)
                    .unwrap();
            }
        }
    }
}

/// A byte rate split into a short number and the unit that scales it,
/// e.g. (14, "B/s"), ("739", "kB/s"), ("1.2", "MB/s").
fn format_rate(bytes_per_sec: u32) -> (String, &'static str) {
    let scaled = |v: f64| {
        if v < 10.0 {
            format!("{v:.1}")
        } else {
            format!("{v:.0}")
        }
    };
    let n = bytes_per_sec as f64;
    if n < 1e3 {
        (bytes_per_sec.to_string(), "B/s")
    } else if n < 1e6 {
        (scaled(n / 1e3), "kB/s")
    } else if n < 1e9 {
        (scaled(n / 1e6), "MB/s")
    } else {
        (scaled(n / 1e9), "GB/s")
    }
}

/// A count sized for the panel: "999", "1.2k", "412k", "1.3M".
fn format_count(n: u32) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => format!("{:.1}k", n as f64 / 1000.0),
        10_000..=999_999 => format!("{}k", n / 1000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// Per-sample columns like `draw_sparkline`, but for unbounded counts:
/// scaled to the window's peak value rather than 0-100. `floor` is the
/// minimum peak, so idle-time jitter doesn't fill the graph.
#[allow(clippy::too_many_arguments)]
fn draw_sparkline_scaled(
    fb: &mut PageTarget<'_, '_>,
    samples: &VecDeque<u32>,
    x0: i32,
    y0: i32,
    width: usize,
    height: i32,
    floor: u32,
    color: Rgb565,
) {
    let peak = samples
        .iter()
        .rev()
        .take(width)
        .copied()
        .max()
        .unwrap_or(0)
        .max(floor) as u64;
    for (i, &value) in samples.iter().rev().take(width).enumerate() {
        let x = x0 + width as i32 - 1 - i as i32;
        let bar = (value as u64 * height as u64).div_ceil(peak) as i32;
        if bar > 0 {
            Rectangle::new(Point::new(x, y0 + height - bar), Size::new(1, bar as u32))
                .into_styled(PrimitiveStyle::with_fill(color))
                .draw(fb)
                .unwrap();
        }
    }
}

/// Root filesystem fullness (a bar — the number barely moves, so no
/// history) over a disk IOPS sparkline.
pub struct DiskPage {
    sampler: Sampler,
}

impl DiskPage {
    pub fn new(sampler: Sampler) -> Self {
        Self { sampler }
    }
}

impl Page for DiskPage {
    fn render(&mut self, fb: &mut PageTarget<'_, '_>, stats: &mut Stats) {
        let (used, total) = stats.disk_used_total();
        let percent = (used * 100).checked_div(total).unwrap_or(0) as u8;
        let fill_color = match percent {
            0..=79 => Rgb565::GREEN,
            80..=89 => Rgb565::YELLOW,
            _ => Rgb565::RED,
        };

        let small = MonoTextStyle::new(CPUS_FONT, Rgb565::WHITE);
        draw_vertical_label(fb, "DISK", &FONT_9X15);
        // Continuous fullness bar: gray track, filled to the used fraction.
        const BAR: Rectangle = Rectangle::new(Point::new(11, 4), Size::new(88, 8));
        BAR.into_styled(PrimitiveStyle::with_fill(GRAY))
            .draw(fb)
            .unwrap();
        let filled = (percent.min(100) as u32 * BAR.size.width) / 100;
        Rectangle::new(BAR.top_left, Size::new(filled, BAR.size.height))
            .into_styled(PrimitiveStyle::with_fill(fill_color))
            .draw(fb)
            .unwrap();
        let text = format!("{}/{}", format_gib(used), format_gib(total));
        Text::with_baseline(&text, Point::new(11, 16), small, Baseline::Top)
            .draw(fb)
            .unwrap();
        let unit_style = MonoTextStyle::new(CPUS_FONT, GRAY);
        let unit_x = 11 + text_width(CPUS_FONT, &text);
        Text::with_baseline("G", Point::new(unit_x, 16), unit_style, Baseline::Top)
            .draw(fb)
            .unwrap();
        // The percent, doubled up from 9x15 — 30px tall, filling the row.
        let text = format!("{percent}%");
        let x = WIDTH as i32 - 2 * text_width(&FONT_9X15, &text);
        draw_scaled_text(fb, &text, &FONT_9X15, Point::new(x, 0), 2, fill_color);

        let history = self.sampler.snapshot();
        draw_sparkline_scaled(
            fb,
            &history.iops,
            IO_GRAPH_X,
            32,
            IO_GRAPH_WIDTH,
            26,
            50,
            IO_COLOR,
        );
        let current = history.iops.back().copied().unwrap_or(0);
        let text = format_count(current);
        let x = WIDTH as i32 - text_width(CPUS_FONT, &text);
        let style = MonoTextStyle::new(CPUS_FONT, IO_COLOR);
        Text::with_baseline(&text, Point::new(x, 41), style, Baseline::Top)
            .draw(fb)
            .unwrap();
        let x = WIDTH as i32 - text_width(CPUS_FONT, "iops");
        let unit_style = MonoTextStyle::new(CPUS_FONT, GRAY);
        Text::with_baseline("iops", Point::new(x, 51), unit_style, Baseline::Top)
            .draw(fb)
            .unwrap();
    }
}

// ---- the network page ----

const NET_GRAPH_X: i32 = 8; // after the vertical IN/OUT labels
/// Network graph width; spans `--net-history` seconds. Inset from both
/// edges so the labels and the rate column never sit on top of bars.
pub const NET_GRAPH_WIDTH: usize = 124;
const NET_IN_COLOR: Rgb565 = Rgb565::new(10, 50, 31); // light blue, like btop's download
const NET_OUT_COLOR: Rgb565 = Rgb565::new(28, 16, 24); // magenta, like btop's upload

/// One half of the mirrored net graph: bars grow away from the midline —
/// up for receive, down for transmit — each half scaled to its own peak
/// (floored at 10kB/s so an idle link stays flat).
fn draw_net_half(
    fb: &mut PageTarget<'_, '_>,
    samples: &VecDeque<u32>,
    mid: i32,
    up: bool,
    height: i32,
    color: Rgb565,
) {
    let peak = samples
        .iter()
        .rev()
        .take(NET_GRAPH_WIDTH)
        .copied()
        .max()
        .unwrap_or(0)
        .max(10_000) as u64;
    for (i, &value) in samples.iter().rev().take(NET_GRAPH_WIDTH).enumerate() {
        let x = NET_GRAPH_X + NET_GRAPH_WIDTH as i32 - 1 - i as i32;
        let bar = (value as u64 * height as u64).div_ceil(peak) as i32;
        if bar > 0 {
            let y = if up { mid - bar } else { mid + 1 };
            Rectangle::new(Point::new(x, y), Size::new(1, bar as u32))
                .into_styled(PrimitiveStyle::with_fill(color))
                .draw(fb)
                .unwrap();
        }
    }
}

/// Network throughput, btop style: one graph split at a center line,
/// receive growing up from it and transmit growing down, labels on the
/// left and the current rates (number over unit) on the right.
pub struct NetPage {
    sampler: Sampler,
}

impl NetPage {
    pub fn new(sampler: Sampler) -> Self {
        Self { sampler }
    }
}

impl Page for NetPage {
    fn render(&mut self, fb: &mut PageTarget<'_, '_>, _stats: &mut Stats) {
        let history = self.sampler.snapshot();
        let mid = PAGE_HEIGHT as i32 / 2;
        Rectangle::new(
            Point::new(NET_GRAPH_X, mid),
            Size::new(NET_GRAPH_WIDTH as u32, 1),
        )
        .into_styled(PrimitiveStyle::with_fill(GRAY))
        .draw(fb)
        .unwrap();
        draw_net_half(fb, &history.net_rx, mid, true, mid, NET_IN_COLOR);
        draw_net_half(
            fb,
            &history.net_tx,
            mid,
            false,
            PAGE_HEIGHT as i32 - mid - 1,
            NET_OUT_COLOR,
        );

        // Vertical labels at the left edge of each half, and the current
        // rate stacked over its unit at the right — all clear of the graph.
        let rows = [
            ("IN", &history.net_rx, NET_IN_COLOR, 5),
            ("OUT", &history.net_tx, NET_OUT_COLOR, mid + 1),
        ];
        let unit_style = MonoTextStyle::new(CPUS_FONT, GRAY);
        for (label, series, color, top) in rows {
            let style = MonoTextStyle::new(CPUS_FONT, color);
            for (i, ch) in label.chars().enumerate() {
                let at = Point::new(0, top + i as i32 * 10);
                Text::with_baseline(&ch.to_string(), at, style, Baseline::Top)
                    .draw(fb)
                    .unwrap();
            }
            let (value, unit) = format_rate(series.back().copied().unwrap_or(0));
            let y = top + 3;
            let x = WIDTH as i32 - text_width(CPUS_FONT, &value);
            Text::with_baseline(&value, Point::new(x, y), style, Baseline::Top)
                .draw(fb)
                .unwrap();
            let x = WIDTH as i32 - text_width(CPUS_FONT, unit);
            Text::with_baseline(unit, Point::new(x, y + 10), unit_style, Baseline::Top)
                .draw(fb)
                .unwrap();
        }
    }
}

// ---- the proxmox page ----

/// Proxmox node status: running/defined VM count and uptime up top, then a
/// btop-style load-average row (scaled as percent of core count, so
/// 1.0/core pins the graph) with the current one-minute load.
pub struct ProxmoxPage {
    sampler: Sampler,
}

impl ProxmoxPage {
    pub fn new(sampler: Sampler) -> Self {
        Self { sampler }
    }
}

impl Page for ProxmoxPage {
    fn render(&mut self, fb: &mut PageTarget<'_, '_>, _stats: &mut Stats) {
        let small = MonoTextStyle::new(CPUS_FONT, Rgb565::WHITE);
        let big = MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE);
        // A two-letter label stacked beside a big value, so the label takes
        // one letter-column and the value gets the large font.
        let stacked = |label: [char; 2], x: i32, fb: &mut PageTarget<'_, '_>| {
            for (i, ch) in label.into_iter().enumerate() {
                let at = Point::new(x, 2 + i as i32 * 10);
                Text::with_baseline(&ch.to_string(), at, small, Baseline::Top)
                    .draw(fb)
                    .unwrap();
            }
        };

        match stats::proxmox_vms() {
            Some(vms) => {
                stacked(['V', 'M'], 0, fb);
                let text = format!("{}/{}", vms.running, vms.total);
                Text::with_baseline(&text, Point::new(10, 2), big, Baseline::Top)
                    .draw(fb)
                    .unwrap();
            }
            None => {
                let gray = MonoTextStyle::new(CPUS_FONT, GRAY);
                Text::with_baseline("not a pve host", Point::new(0, 8), gray, Baseline::Top)
                    .draw(fb)
                    .unwrap();
            }
        }
        let time = stats::uptime_text();
        let time_x = WIDTH as i32 - 8 - text_width(&FONT_10X20, &time);
        Text::with_baseline(&time, Point::new(time_x, 2), big, Baseline::Top)
            .draw(fb)
            .unwrap();
        stacked(['U', 'P'], WIDTH as i32 - 6, fb);

        // The load row, styled like a cpus-page row: a vertical LOAD label
        // in the tiny font (four letters fit the row), spike graph, value.
        let history = self.sampler.snapshot();
        let tiny = MonoTextStyle::new(&FONT_4X6, Rgb565::WHITE);
        for (i, ch) in "LOAD".chars().enumerate() {
            let at = Point::new(0, 32 + i as i32 * 7);
            Text::with_baseline(&ch.to_string(), at, tiny, Baseline::Top)
                .draw(fb)
                .unwrap();
        }
        draw_sparkline(fb, &history.load, 8, 32, GRAPH_WIDTH, 26, usage_color);
        let load = stats::load_average();
        let text = if load < 10.0 {
            format!("{load:.2}")
        } else {
            format!("{load:.1}")
        };
        let pct = history.load.back().copied().unwrap_or(0);
        let x = WIDTH as i32 - text_width(CPUS_FONT, &text);
        let style = MonoTextStyle::new(CPUS_FONT, usage_color(pct));
        Text::with_baseline(&text, Point::new(x, 40), style, Baseline::Top)
            .draw(fb)
            .unwrap();
    }
}

// ---- the warnings page ----

/// Host problems: Pi throttling flags (under-voltage, frequency caps),
/// overheating, disks filling up. Severe (happening-now) problems in red,
/// cautions in yellow. The page removes itself from the rotation while
/// everything is clear.
pub struct WarningsPage;

impl Page for WarningsPage {
    fn active(&mut self, stats: &mut Stats) -> bool {
        !stats.warnings().is_empty()
    }

    fn render(&mut self, fb: &mut PageTarget<'_, '_>, stats: &mut Stats) {
        let warnings = stats.warnings();
        if warnings.is_empty() {
            // Only reachable when the page is forced (a solo rotation).
            let gray = MonoTextStyle::new(CPUS_FONT, GRAY);
            Text::with_baseline("all clear", Point::new(0, 26), gray, Baseline::Top)
                .draw(fb)
                .unwrap();
            return;
        }
        for (i, warning) in warnings.iter().take(6).enumerate() {
            let color = if warning.severe {
                Rgb565::RED
            } else {
                Rgb565::YELLOW
            };
            let style = MonoTextStyle::new(CPUS_FONT, color);
            Text::with_baseline(
                &warning.text,
                Point::new(0, i as i32 * 10),
                style,
                Baseline::Top,
            )
            .draw(fb)
            .unwrap();
        }
    }
}
