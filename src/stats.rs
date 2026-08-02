//! Local host stats, ported from `hardware/rpiInfo/rpiInfo.c`.
//!
//! The C version shelled out to `top` and `df`; here CPU and memory come
//! straight from /proc, and disk usage from statvfs(2). This module is the
//! piece that will eventually be swapped for the Proxmox API client.

use std::fs;
use std::io;
use std::net::UdpSocket;
use std::thread;
use std::time::Duration;

/// CPU usage from /proc/stat deltas. Keeps the previous sample between
/// calls, so each reading covers the interval since the last one and never
/// has to sleep mid-render (which would stall the display).
pub struct CpuSampler {
    last: Option<(u64, u64)>, // (total, idle) jiffies
}

impl CpuSampler {
    pub fn new() -> Self {
        Self { last: None }
    }

    pub fn percent(&mut self) -> io::Result<u8> {
        let mut current = Self::sample()?;
        let last = match self.last {
            Some(last) => last,
            None => {
                // First reading: no history yet, take a short window.
                let first = current;
                thread::sleep(Duration::from_millis(100));
                current = Self::sample()?;
                first
            }
        };
        self.last = Some(current);

        let total = current.0.saturating_sub(last.0);
        let idle = current.1.saturating_sub(last.1);
        if total == 0 {
            return Ok(0);
        }
        Ok((100 * (total - idle) / total) as u8)
    }

    fn sample() -> io::Result<(u64, u64)> {
        let stat = fs::read_to_string("/proc/stat")?;
        let line = stat
            .lines()
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "/proc/stat is empty"))?;
        // "cpu  user nice system idle iowait irq softirq steal ..."
        let fields: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|f| f.parse().ok())
            .collect();
        let total: u64 = fields.iter().sum();
        let idle = fields.get(3).copied().unwrap_or(0) + fields.get(4).copied().unwrap_or(0);
        Ok((total, idle))
    }
}

/// Memory usage percentage from /proc/meminfo, matching the C version's
/// (MemTotal - MemFree) / MemTotal calculation.
pub fn memory_percent() -> io::Result<u8> {
    let meminfo = fs::read_to_string("/proc/meminfo")?;
    let field = |name: &str| -> Option<u64> {
        meminfo
            .lines()
            .find(|l| l.starts_with(name))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    let total = field("MemTotal:")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "MemTotal not found"))?;
    let free = field("MemFree:").unwrap_or(0);
    Ok((100 * (total - free) / total) as u8)
}

/// SoC temperature in whole degrees Celsius.
pub fn temperature_celsius() -> io::Result<u8> {
    let raw = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp")?;
    let millidegrees: i64 = raw
        .trim()
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok((millidegrees / 1000).clamp(0, u8::MAX as i64) as u8)
}

/// Root filesystem usage percentage via statvfs(2).
pub fn disk_percent() -> io::Result<u8> {
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c"/".as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let total = stat.f_blocks as u64;
    let free = stat.f_bfree as u64;
    if total == 0 {
        return Ok(0);
    }
    Ok((100 * (total - free) / total) as u8)
}

/// Fully qualified hostname: the kernel hostname, extended to an FQDN via
/// /etc/hosts if it's a short name (the usual Proxmox setup).
pub fn fqdn() -> String {
    let short = fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    if short.contains('.') {
        return short;
    }
    if let Ok(hosts) = fs::read_to_string("/etc/hosts") {
        for line in hosts.lines() {
            let line = line.split('#').next().unwrap_or("");
            for token in line.split_whitespace().skip(1) {
                if token.strip_prefix(short.as_str()).is_some_and(|rest| rest.starts_with('.')) {
                    return token.to_string();
                }
            }
        }
    }
    short
}

/// Primary IPv4 address, discovered by "connecting" a UDP socket toward a
/// public address (no packets are sent) and reading the chosen local address.
pub fn ip_address() -> String {
    let addr = UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string());
    addr.unwrap_or_else(|_| "xxx.xxx.xxx.xxx".to_string())
}
