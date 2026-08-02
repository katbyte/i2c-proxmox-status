# i2c-proxmox-status

A straight Rust conversion of the C display daemon from
[UCTRONICS/SKU_RM0004](https://github.com/UCTRONICS/SKU_RM0004), the firmware
companion for the UCTRONICS Raspberry Pi 1U rack mount. The original C and
Python sources remain in git history.

## What it does

Cycles the front-panel LCD through four status screens — CPU load, RAM usage,
SoC temperature, and disk usage — one every two seconds, with the host's IP
address in a header line. Stats are read locally from `/proc`, sysfs, and
statvfs(2).

The end goal is to show Proxmox host stats pulled from the Proxmox API
instead of local readings; that part isn't built yet.

## The display

The rack mount's front panel is a 0.96" 160x80 color LCD driven by an ST7735
controller. The Pi doesn't talk to the ST7735 directly: an RP2040 on the
board bridges I2C to the panel, appearing at address `0x18` on `/dev/i2c-1`.
Every write is a 3-byte `[register, high, low]` transaction against the
bridge's register map (coordinate, data, burst, and sync registers) — so
generic SPI ST7735 drivers don't apply here.

## Build and run

Needs Rust 1.85+ (`apt install cargo` on Debian 13 is enough) and I2C enabled
on the Pi.

```bash
cargo build --release
sudo ./target/release/i2c-proxmox-status   # sudo unless your user is in the i2c group
```

## Layout

- `src/st7735.rs` — display driver (the board's I2C register protocol, not raw ST7735)
- `src/fonts.rs` — bitmap fonts, generated from the original C `fonts.c`
- `src/stats.rs` — stat collection (`/proc`, sysfs, statvfs)
- `src/screens.rs` — `Screen` trait and the four status screens
- `src/main.rs` — the display loop
