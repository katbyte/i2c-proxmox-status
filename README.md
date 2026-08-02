# i2c-proxmox-status

A straight Rust conversion of the C display daemon from
[UCTRONICS/SKU_RM0004](https://github.com/UCTRONICS/SKU_RM0004), the firmware
companion for the UCTRONICS Pi Rack Pro (19" 1U rack mount for Raspberry Pi,
SKU RM0004). The original C and Python sources remain in git history.

## What it does

Cycles the front-panel LCD through four status screens — CPU load, RAM usage,
SoC temperature, and disk usage — one every two seconds. A header line shows
`ip - hostname.fqdn`, scrolling as a slow marquee when it's too wide for the
display. Stats are read locally from `/proc`, sysfs, and statvfs(2).

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

Single-pixel writes are slow (one I2C transaction each), so rectangle fills
use burst mode: enable register `0x01`, stream raw pixel bytes in 160-byte
chunks, disable, sync. Text rendering uses pixel-at-a-time writes, which is
why the screens only repaint the regions that change.

### Fonts

`src/fonts.rs` holds four bitmap fonts (7x10, 8x16, 11x18, 16x26), generated
from the original repo's `fonts.c`. Each font covers ASCII 32–126; a glyph is
`height` u16 values, one per row, with bit 15 as the leftmost pixel. Drawing
a character sets a glyph-sized address window and streams foreground or
background color per bit.

### Screens

`src/screens.rs` implements the header and the rotation. The static
background (black fill plus the blue divider bar) is painted once at
startup; after that the header and each screen repaint only their own
regions, so the display never visibly wipes. The header marquee is
rasterized into an offscreen buffer and blitted in one burst per scroll
step; each stat screen repaints its value line and gauge. The gauge is ten
6x10 segments at the bottom, filled proportionally to the percentage in a
per-screen color (green/yellow/red/blue), gray for the remainder.

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

## Layout

- `src/st7735.rs` — display driver (the bridge's I2C register protocol)
- `src/fonts.rs` — bitmap fonts, generated from the original C `fonts.c`
- `src/stats.rs` — stat collection (`/proc`, sysfs, statvfs)
- `src/screens.rs` — `Screen` trait and the four status screens
- `src/main.rs` — the display loop
