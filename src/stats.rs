//! Local host stats via the `sysinfo` crate, plus IP/hostname helpers.
//!
//! One `Stats` instance is shared by all screens: sysinfo computes CPU usage
//! as the delta since the previous refresh, so the state has to persist
//! across renders. This module is the piece that will eventually be joined
//! by the Proxmox API client.

use std::fs;
use std::net::UdpSocket;

use sysinfo::{Components, Disks, MemoryRefreshKind, System};

pub struct Stats {
    system: System,
    components: Components,
    disks: Disks,
}

impl Stats {
    pub fn new() -> Self {
        let mut system = System::new();
        // Seed the CPU counters so the first real reading has a baseline.
        system.refresh_cpu_usage();
        Self {
            system,
            components: Components::new_with_refreshed_list(),
            disks: Disks::new_with_refreshed_list(),
        }
    }

    /// CPU usage since the previous call, across all cores.
    pub fn cpu_percent(&mut self) -> u8 {
        self.system.refresh_cpu_usage();
        self.system.global_cpu_usage().round() as u8
    }

    /// Used memory as sysinfo reports it (total minus available).
    pub fn memory_percent(&mut self) -> u8 {
        self.system.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        let total = self.system.total_memory();
        if total == 0 {
            return 0;
        }
        (self.system.used_memory() * 100 / total) as u8
    }

    /// SoC temperature in whole degrees Celsius: the CPU thermal component,
    /// or the first component that reports anything.
    pub fn temperature_celsius(&mut self) -> u8 {
        self.components.refresh(true);
        let cpu_first = |c: &&sysinfo::Component| c.label().to_lowercase().contains("cpu");
        self.components
            .iter()
            .find(cpu_first)
            .or_else(|| self.components.iter().next())
            .and_then(|c| c.temperature())
            .map(|t| t.round().clamp(0.0, 255.0) as u8)
            .unwrap_or(0)
    }

    /// Root filesystem usage percentage.
    pub fn disk_percent(&mut self) -> u8 {
        self.disks.refresh(true);
        let Some(root) = self
            .disks
            .iter()
            .find(|d| d.mount_point() == std::path::Path::new("/"))
        else {
            return 0;
        };
        let total = root.total_space();
        if total == 0 {
            return 0;
        }
        ((total - root.available_space()) * 100 / total) as u8
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_smoke() {
        let mut stats = Stats::new();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let cpu = stats.cpu_percent();
        let mem = stats.memory_percent();
        let disk = stats.disk_percent();
        let temp = stats.temperature_celsius();
        println!("cpu={cpu}% mem={mem}% disk={disk}% temp={temp}C ip={} fqdn={}", ip_address(), fqdn());
        assert!(cpu <= 100);
        assert!((1..=100).contains(&mem));
        assert!(disk <= 100);
    }
}
