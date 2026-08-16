# i2c-proxmox-status

A straight Rust conversion of the C display daemon from
[UCTRONICS/SKU_RM0004](https://github.com/UCTRONICS/SKU_RM0004), the firmware
companion for the UCTRONICS Pi Rack Pro (19" 1U rack mount for Raspberry Pi,
SKU RM0004). The original C and Python sources remain in git history.

## What it does

Rotates the front-panel LCD through a configurable set of pages, one every
seven seconds, under a header line paging between the IP and the hostname.
Stats come from the [sysinfo](https://crates.io/crates/sysinfo) crate.

Pages (`--pages`, comma separated, in rotation order):

- `cpus` — btop-style per-core view: a total-CPU history sparkline on top,
  then a sparkline and current percent per core, every sample colored on
  btop's green→yellow→red gradient by load. A background thread samples
  usage continuously so the history keeps moving while other pages show;
  `--page-history <secs>` sets how much history spans the graphs (default
  2 minutes, one sample per pixel column)
- `temps` — CPU/SoC and NVMe temperatures, one row per sensor: the same
  history sparkline plus a big current reading in the large font, both
  colored by status (green under 50°C, yellow to 65°C, orange to 75°C, red
  above). Sensors are matched from the kernel's hwmon labels (cpu_thermal /
  coretemp / k10temp for the CPU, the NVMe Composite sensor); rows only
  appear for sensors that exist. Temperatures drift slowly, so this page
  gets its own, much longer window: `--temp-history <secs>` (default 1 hour)
- `mem` — memory usage: a full-height history sparkline on the load
  gradient (`--mem-history` window, default 1 hour), the big current
  percent, and used over total on two lines
- `disk` — root filesystem fullness as a bar with the big percent and
  used/total (no history — the number barely moves), over a disk IOPS
  sparkline scaled to the window's peak (whole physical disks summed from
  /proc/diskstats); `--io-history <secs>` sets its window (default 5
  minutes)
- `network` — throughput in btop's style: one graph split at a center
  line, receive climbing up from it and transmit hanging down, each half
  auto-scaled to its own peak, with labels on the left and the current
  rates (number over unit) on the right. Physical interfaces only
  (en*/eth*/wl* — bridges and taps mirror the same packets, so counting
  them would double everything); `--net-history` sets the window (default
  and minimum 5 minutes)
- `warnings` — host problems, one line each: the Pi firmware's throttling
  flags (under-voltage, frequency capping, throttling, soft temp limit —
  current ones red, since-boot ones yellow), overheating sensors, disks
  ≥90% and RAM ≥95%. The page removes itself from the rotation while
  everything is clear, so seeing it at all means something's wrong
- `proxmox` — running/defined VM count for the local node (read straight
  off /etc/pve and the qemu pidfiles, no API or auth needed) with uptime,
  over a cpus-style load-average history row (scaled as percent of core
  count, so 1.0 per core pins the graph)
- `uctronics-cpu`, `uctronics-ram`, `uctronics-temp`, `uctronics-disk` —
  the single-stat gauge screens carried over from the original C firmware

The default rotation is `cpus,temps,mem,disk,network,proxmox,warnings`.
Every page holds for
`--page-hold` seconds (default 7), counted from when the page is fully on
the glass. While a page is up,
its live readings and graphs redraw at most once per `--page-refresh`
seconds (default 1); a refresh due on the same frame as a header change
waits one frame, so the header's flush gets the bus to itself.

On exit the daemon leaves a full-screen message on the panel (the bridge
MCU keeps showing the last frame after the process stops): `--close-text`
(default `CLOSED`, white) on a clean exit via ctrl-c/SIGTERM, and
`--crash-text` (default `CRASH`, red) if the daemon panics.

Page content is treated as static while shown — the diff flush means an
unchanged page costs nothing on the slow bus. `--page-mode
<cut|wipe-up|wipe-down|wipe-left|wipe-right>` picks the transition between
pages: `cut` (default) switches instantly; the wipes slide the old page out
and the next in over a few large steps, since every step redraws the whole
page area (~450ms each on the real bus).

The header text doubles as a status light: white while the host is
healthy, then yellow / orange / red as the hottest temperature sensor
(65/75/85°C) or root filesystem fullness (80/90/95%) escalates. Active Pi
throttling flags go straight to red; since-boot flags show yellow.

Header behavior is configurable:

- `--host <hostname|fqdn>` — short hostname (default) or the FQDN
- `--header-mode <cut|slide|wipe-up|wipe-down|wipe-left|wipe-right>` — the
  cut and wipe modes page between the IP and the hostname (a part still too
  wide is further split to the display width), holding each for
  `--header-hold` seconds (default 10). `cut` (the default) swaps
  instantly — no animation, so the only bus cost is one band flush per
  swap, deferred while a page transition is animating; the wipes slide to
  the next page in the given direction. `slide` shows everything at once,
  as a continuous 1px marquee when too wide (which re-sends the header band
  every frame, competing with page updates for the bus)
- `--header-align <left|center|right>` — where text sits when it doesn't
  fill the width (default `center`; `centre` works too)
- `--header-sync` — the header changes only at the moment the page
  rotation changes (once `--header-hold` has elapsed), so with matching
  wipe modes the header and page slide in step

The end goal is to show Proxmox host stats pulled from the Proxmox API
instead of local readings; that part isn't built yet.

## How the display works

Each Pi tray on the rack has a 0.96" 160x80 color LCD. The panel itself is
driven by an ST7735 controller, but the Pi never talks to the ST7735
directly — an on-board microcontroller (which also handles the power button
and safe shutdown) owns the panel and exposes a small register interface to
the Pi over I2C at address `0x18`. `i2cdetect -y 1` showing a device at
`0x18` confirms this variant of the hardware.

### The bridge protocol

Every transaction the Pi sends is three bytes — `[register, high, low]`:

| Register | Purpose |
|----------|---------|
| `0x2A`   | X window: start column in `high`, end column in `low` |
| `0x2B`   | Y window: start row in `high`, end row in `low` |
| `0x2C`   | commit the window |
| `0x00`   | write one pixel: RGB565 color split into `high`/`low` |
| `0x01`   | burst mode on (`low=1`) / off (`low=0`) |
| `0x03`   | sync — tells the MCU to flush to the panel |

Drawing means: set an address window (a rectangle), then stream one RGB565
value per pixel into it, left-to-right, top-to-bottom. The register numbers
`0x2A`/`0x2B`/`0x2C` mirror the real ST7735 CASET/RASET/RAMWR commands the
MCU issues on the other side.

Two quirks the driver has to honor:

- **Panel offset** — the 160x80 module is a windowed slice of the ST7735's
  native 162x132 RAM, mounted rotated. In this orientation every Y
  coordinate is offset by 24 rows (`Y_START` in `src/st7735.rs`).
- **Pacing** — the MCU needs breathing room: ~10µs after each 3-byte
  transaction, and burst writes chunked to 160 bytes with ~700µs between
  chunks.
- **Session size** — a single burst session longer than ~5KB (16 full-width
  rows) overruns the bridge and the content lands displaced on the panel,
  so tall blits are split into 16-row windows.

Single-pixel writes (register `0x00`) cost one I2C transaction per pixel,
so this daemon never uses them: everything goes through burst mode — enable
register `0x01`, stream raw pixel bytes in 160-byte chunks, disable, sync.

### Framebuffer and differential flushing

The I2C bus is the bottleneck: ~40 KB/s at 400kHz means a full-screen push
(25.6 KB) takes ~0.8s, and a 160x16 header band ~140ms — about 7fps at best.
So nothing draws to the display directly. Each frame is composed into an
in-memory framebuffer (`src/framebuffer.rs`), which implements
[embedded-graphics](https://crates.io/crates/embedded-graphics)'
`DrawTarget`, so text, rectangles, and the rest of its primitives all render
into RAM for free. `flush()` then diffs the frame against a copy of what the
display currently shows and pushes only the changed rows (contiguous dirty
rows as one full-width burst blit each — the bridge MCU misbehaves with
narrow windows at arbitrary offsets, so bands are never column-trimmed).

Fonts come from embedded-graphics' built-in monospace set (9x15 for the
header, 10x20 for the values) — the bitmap fonts hand-ported from the C repo
are gone.

### Pages

`src/screens.rs` implements the header and the pages. When `ip - hostname`
is wider than the display the header animates per `--header-mode`: the
slide marquee advances 1px per frame, while the wipe modes page through
display-width chunks of the text. Each `Page` renders into the 160x55 area
under the divider through a clipped+translated draw target, so pages draw
in local coordinates from (0,0) and the same render code works mid-wipe at
an offset. Main clears the page area and re-renders the current page every
frame; the diff flush works out what actually needs to hit the bus. The gauge is ten 6x10 segments at the bottom, filled proportionally to
the percentage in a per-screen color (green/yellow/red/blue), gray for the
remainder.

## Build and run

Needs Rust 1.85+ (`apt install cargo` on Debian 13 is enough) and I2C
enabled on the Pi — in `/boot/firmware/config.txt` (or `/boot/config.txt` on
older OS releases):

```
dtparam=i2c_arm=on,i2c_arm_baudrate=400000
```

Then:

```bash
cargo build --release
sudo ./target/release/i2c-proxmox-status   # sudo unless your user is in the i2c group
```

Dependencies are vendored: all crate sources live in `vendor/` and
`.cargo/config.toml` points builds at it, so no network access is needed.
After changing anything in `Cargo.toml`, refresh it with `cargo vendor`.

## Simulator

Preview the display on a desktop without the hardware:

```bash
cargo run --features simulator -- --simulator   # or -s; needs SDL2 (brew install sdl2 / apt install libsdl2-dev)
```

The cargo feature controls whether SDL2 is linked at all (so the Pi build
never needs it); the `--simulator`/`-s` flag selects it at runtime.

Opens a 4x-scaled window driven by the exact same compose/flush path as the
real panel. Blits sleep through a timing model of the I2C bus (wire time,
chunk pauses, session command overhead), so the marquee cadence and
screen-switch hitches match what the rack shows — design against the
simulator, then just rebuild on the Pi. The model is a close approximation
of measured hardware behavior, not a cycle-exact emulation.

## Layout

- `src/st7735.rs` — display transport (the bridge's I2C register protocol)
- `src/sim.rs` — SDL simulator transport with an I2C timing model (`--features simulator`)
- `src/framebuffer.rs` — embedded-graphics `DrawTarget` + differential flush
- `src/stats.rs` — stat collection (sysinfo crate), the stat history sampler thread, Proxmox guest counts, IP/hostname helpers
- `src/screens.rs` — header animations, `Page` trait, the pages
- `src/main.rs` — the compose/flush loop
