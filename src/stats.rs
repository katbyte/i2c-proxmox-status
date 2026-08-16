//! Local host stats via the `sysinfo` crate, plus IP/hostname helpers.
//!
//! One `Stats` instance is shared by all screens: sysinfo computes CPU usage
//! as the delta since the previous refresh, so the state has to persist
//! across renders. This module is the piece that will eventually be joined
//! by the Proxmox API client.

use std::collections::VecDeque;
use std::fs;
use std::net::UdpSocket;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use sysinfo::{Components, Disks, MemoryRefreshKind, Networks, System};

/// More samples than any graph has columns (the panel is 160px wide);
/// pages render the newest `width` samples of a series, so the span a graph
/// shows is set purely by that series' sampling interval.
const HISTORY_CAP: usize = 160;

/// Rolling stat history, oldest sample first. Each series advances on its
/// own interval so different pages can span different windows.
#[derive(Clone, Default)]
pub struct History {
    pub total: VecDeque<u8>,
    pub cores: Vec<VecDeque<u8>>,
    pub temps: Vec<TempSeries>,
    pub mem: VecDeque<u8>,
    pub iops: VecDeque<u32>,
    /// One-minute load average as a percent of core count (1.0/core = 100).
    pub load: VecDeque<u8>,
    /// Network receive/transmit rates in bytes per second.
    pub net_rx: VecDeque<u32>,
    pub net_tx: VecDeque<u32>,
}

/// One tracked temperature sensor; only sensors present at startup get a
/// series, so pages can lay out rows from `temps` alone.
#[derive(Clone)]
pub struct TempSeries {
    pub label: &'static str,
    pub samples: VecDeque<u8>,
}

/// Per-metric sampling intervals; each is `window seconds / graph columns`
/// so one sample lands per column and the graph spans exactly the window.
pub struct SamplerConfig {
    pub cpu: Duration,
    pub temp: Duration,
    pub mem: Duration,
    pub io: Duration,
    pub net: Duration,
}

/// Handle to the background thread that samples CPU usage, temperatures,
/// memory and disk IO — each on its own interval — so the history keeps
/// moving no matter which page is showing. Clones share the same history.
#[derive(Clone)]
pub struct Sampler {
    shared: Arc<Mutex<History>>,
}

impl Sampler {
    pub fn start(cfg: SamplerConfig) -> Self {
        let shared = Arc::new(Mutex::new(History::default()));
        let sink = Arc::clone(&shared);
        thread::spawn(move || sample_loop(cfg, sink));
        Self { shared }
    }

    pub fn snapshot(&self) -> History {
        self.shared.lock().unwrap().clone()
    }
}

fn sample_loop(cfg: SamplerConfig, sink: Arc<Mutex<History>>) {
    let mut system = System::new();
    // Seed the counters so the first sample has a baseline.
    system.refresh_cpu_usage();
    let mut components = Components::new_with_refreshed_list();
    let mut prev_ops = read_disk_ops();
    let mut networks = Networks::new_with_refreshed_list();
    let mut prev_net = physical_net_totals(&networks);
    {
        let (cpu, nvme) = sensor_temps(&components);
        let mut history = sink.lock().unwrap();
        for (label, reading) in [("CPU", cpu), ("NVME", nvme)] {
            if let Some(value) = reading {
                // Seed with the current reading so the graph isn't empty
                // until the first (possibly minutes-away) interval tick.
                history.temps.push(TempSeries { label, samples: VecDeque::from([value]) });
            }
        }
        // Memory gets the same seeding — its window is measured in hours.
        system.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        let total = system.total_memory();
        if total > 0 {
            push_capped(&mut history.mem, (system.used_memory() * 100 / total) as u8);
        }
    }

    let now = Instant::now();
    let mut next_cpu = now + cfg.cpu;
    let mut next_temp = now + cfg.temp;
    let mut next_mem = now + cfg.mem;
    let mut next_io = now + cfg.io;
    let mut next_net = now + cfg.net;
    // Advance a deadline by its interval, resyncing if sampling fell behind.
    let bump = |deadline: &mut Instant, interval: Duration| {
        *deadline += interval;
        if *deadline < Instant::now() {
            *deadline = Instant::now() + interval;
        }
    };

    loop {
        let next = next_cpu.min(next_temp).min(next_mem).min(next_io).min(next_net);
        thread::sleep(next.saturating_duration_since(Instant::now()));
        let now = Instant::now();

        if now >= next_cpu {
            system.refresh_cpu_usage();
            let mut history = sink.lock().unwrap();
            history.cores.resize_with(system.cpus().len(), VecDeque::new);
            push_capped(&mut history.total, system.global_cpu_usage().round() as u8);
            for (core, samples) in system.cpus().iter().zip(&mut history.cores) {
                push_capped(samples, core.cpu_usage().round() as u8);
            }
            let cores = system.cpus().len().max(1) as f64;
            let load_pct = (System::load_average().one * 100.0 / cores).clamp(0.0, 255.0);
            push_capped(&mut history.load, load_pct.round() as u8);
            bump(&mut next_cpu, cfg.cpu);
        }
        if now >= next_temp {
            components.refresh(true);
            let (cpu, nvme) = sensor_temps(&components);
            let mut history = sink.lock().unwrap();
            for series in &mut history.temps {
                let reading = if series.label == "CPU" { cpu } else { nvme };
                // A dropped reading repeats the last value rather than
                // spiking the graph to zero.
                let value = reading.or_else(|| series.samples.back().copied()).unwrap_or(0);
                push_capped(&mut series.samples, value);
            }
            bump(&mut next_temp, cfg.temp);
        }
        if now >= next_mem {
            system.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
            let total = system.total_memory();
            let percent =
                if total == 0 { 0 } else { (system.used_memory() * 100 / total) as u8 };
            push_capped(&mut sink.lock().unwrap().mem, percent);
            bump(&mut next_mem, cfg.mem);
        }
        if now >= next_io {
            let ops = read_disk_ops();
            let iops = match (prev_ops, ops) {
                (Some(prev), Some(cur)) => {
                    (cur.saturating_sub(prev) as f64 / cfg.io.as_secs_f64()).round() as u32
                }
                _ => 0,
            };
            prev_ops = ops;
            push_capped_u32(&mut sink.lock().unwrap().iops, iops);
            bump(&mut next_io, cfg.io);
        }
        if now >= next_net {
            networks.refresh(true);
            let (rx, tx) = physical_net_totals(&networks);
            let secs = cfg.net.as_secs_f64();
            let rx_rate = (rx.saturating_sub(prev_net.0) as f64 / secs).round() as u32;
            let tx_rate = (tx.saturating_sub(prev_net.1) as f64 / secs).round() as u32;
            prev_net = (rx, tx);
            let mut history = sink.lock().unwrap();
            push_capped_u32(&mut history.net_rx, rx_rate);
            push_capped_u32(&mut history.net_tx, tx_rate);
            drop(history);
            bump(&mut next_net, cfg.net);
        }
    }
}

/// Cumulative received/transmitted bytes summed over physical interfaces
/// only (en*/eth*/wl*). Bridges (vmbr*), taps and veths mirror the same
/// packets on a Proxmox host, so counting them would double everything.
fn physical_net_totals(networks: &Networks) -> (u64, u64) {
    networks
        .iter()
        .filter(|(name, _)| ["en", "eth", "wl"].iter().any(|p| name.starts_with(p)))
        .fold((0, 0), |(rx, tx), (_, data)| {
            (rx + data.total_received(), tx + data.total_transmitted())
        })
}

/// Completed read+write operations summed across physical disks, from
/// /proc/diskstats (fields 4 and 8). Partitions, loops, zvols and
/// device-mapper entries are skipped so IO isn't counted twice.
fn read_disk_ops() -> Option<u64> {
    let stats = fs::read_to_string("/proc/diskstats").ok()?;
    let mut ops = 0u64;
    for line in stats.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 || !is_whole_disk(fields[2]) {
            continue;
        }
        let reads: u64 = fields[3].parse().unwrap_or(0);
        let writes: u64 = fields[7].parse().unwrap_or(0);
        ops += reads + writes;
    }
    Some(ops)
}

/// Whether a /proc/diskstats device name is a whole physical disk (sda,
/// vdb, nvme0n1, mmcblk0) rather than a partition or virtual device.
fn is_whole_disk(name: &str) -> bool {
    if let Some(rest) = name.strip_prefix("nvme") {
        return !rest.contains('p'); // nvme0n1 yes, nvme0n1p2 no
    }
    if let Some(rest) = name.strip_prefix("mmcblk") {
        return rest.chars().all(|c| c.is_ascii_digit());
    }
    for prefix in ["sd", "vd", "hd"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            return rest.chars().all(|c| c.is_ascii_alphabetic());
        }
    }
    false
}

/// CPU and NVMe temperatures, with a mock fallback for simulator builds:
/// desktops previewing the panel often expose no hwmon sensors at all, so
/// synthesize plausible wandering readings rather than an empty page.
fn sensor_temps(components: &Components) -> (Option<u8>, Option<u8>) {
    let (cpu, nvme) = read_temps(components);
    if cfg!(feature = "simulator") && cpu.is_none() && nvme.is_none() {
        return mock_temps();
    }
    (cpu, nvme)
}

/// Fake but believable sensor readings: slow sine drifts plus a faster
/// wiggle, distinct periods per sensor so the graphs don't move in step.
fn mock_temps() -> (Option<u8>, Option<u8>) {
    let t = std::time::UNIX_EPOCH.elapsed().unwrap_or_default().as_secs_f64();
    let cpu = 55.0 + 9.0 * (t / 990.0).sin() + 5.0 * (t / 97.0).sin() + 2.0 * (t / 13.0).sin();
    let nvme = 42.0 + 5.0 * (t / 1370.0).sin() + 3.0 * (t / 151.0).sin();
    (Some(cpu.round() as u8), Some(nvme.round() as u8))
}

/// CPU/SoC and NVMe temperatures from the sensor list. lm-sensors naming:
/// the CPU shows up as cpu_thermal (Pi), coretemp/"Package id" (Intel ISA)
/// or k10temp (AMD); NVMe drives report a "Composite" sensor.
fn read_temps(components: &Components) -> (Option<u8>, Option<u8>) {
    let mut cpu = None;
    let mut nvme = None;
    for component in components.iter() {
        let label = component.label().to_lowercase();
        let Some(temp) = component.temperature() else {
            continue;
        };
        let temp = temp.round().clamp(0.0, 255.0) as u8;
        if cpu.is_none()
            && (label.contains("cpu")
                || label.contains("coretemp")
                || label.contains("k10temp")
                || label.contains("package"))
        {
            cpu = Some(temp);
        } else if nvme.is_none() && (label.contains("nvme") || label.contains("composite")) {
            nvme = Some(temp);
        }
    }
    (cpu, nvme)
}

fn push_capped(samples: &mut VecDeque<u8>, value: u8) {
    if samples.len() >= HISTORY_CAP {
        samples.pop_front();
    }
    samples.push_back(value);
}

fn push_capped_u32(samples: &mut VecDeque<u32>, value: u32) {
    if samples.len() >= HISTORY_CAP {
        samples.pop_front();
    }
    samples.push_back(value);
}

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

    /// Current CPU and NVMe temperatures, read now — the temps history
    /// advances far too slowly to serve as "the current reading".
    pub fn temps_now(&mut self) -> (Option<u8>, Option<u8>) {
        self.components.refresh(true);
        sensor_temps(&self.components)
    }

    /// Used and total memory in bytes.
    pub fn memory_used_total(&mut self) -> (u64, u64) {
        self.system.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        (self.system.used_memory(), self.system.total_memory())
    }

    /// Root filesystem usage percentage.
    pub fn disk_percent(&mut self) -> u8 {
        let (used, total) = self.disk_used_total();
        if total == 0 {
            return 0;
        }
        (used * 100 / total) as u8
    }

    /// Used and total space on the root filesystem, in bytes.
    pub fn disk_used_total(&mut self) -> (u64, u64) {
        self.disks.refresh(true);
        let Some(root) = self.disks.iter().find(|d| d.mount_point() == Path::new("/")) else {
            return (0, 0);
        };
        let total = root.total_space();
        (total - root.available_space(), total)
    }
}

/// Running/defined guest counts read straight off the Proxmox host's
/// filesystem — no API client or auth needed for the local node.
pub struct GuestCounts {
    pub running: usize,
    pub total: usize,
}

/// VM counts for the local node, or `None` when this isn't a Proxmox host
/// (no /etc/pve). Definitions come from the pmxcfs config dir; a VM is
/// running if qemu wrote its pidfile.
pub fn proxmox_vms() -> Option<GuestCounts> {
    if !Path::new("/etc/pve").exists() {
        return None;
    }
    let Ok(entries) = fs::read_dir("/etc/pve/qemu-server") else {
        return Some(GuestCounts { running: 0, total: 0 });
    };
    let ids: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            Some(name.strip_suffix(".conf")?.to_string())
        })
        .collect();
    let running = ids
        .iter()
        .filter(|id| Path::new(&format!("/var/run/qemu-server/{id}.pid")).exists())
        .count();
    Some(GuestCounts { running, total: ids.len() })
}

/// Host uptime formatted to fit the panel: "4d3h", "7h12m", or "42m".
pub fn uptime_text() -> String {
    let secs = System::uptime();
    let (days, hours, minutes) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else {
        format!("{minutes}m")
    }
}

/// One-minute load average.
pub fn load_average() -> f64 {
    System::load_average().one
}

/// The Pi firmware's throttling bitmask (0 where unavailable, e.g. x86).
/// Bits 0-3: under-voltage / frequency capped / throttled / soft temp
/// limit right now; bits 16-19 are the same events since boot.
pub fn throttled_flags() -> u32 {
    fs::read_to_string("/sys/devices/platform/soc/soc:firmware/get_throttled")
        .ok()
        .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
        .unwrap_or(0)
}

/// An active host problem for the warnings page: `severe` problems are the
/// happening-right-now kind, the rest are worth-knowing (since-boot flags,
/// approaching thresholds).
pub struct Warning {
    pub text: String,
    pub severe: bool,
}

impl Stats {
    /// Everything currently wrong with the host, worst first. Empty on a
    /// healthy box — the warnings page hides itself then.
    pub fn warnings(&mut self) -> Vec<Warning> {
        let mut severe = Vec::new();
        let mut minor = Vec::new();

        let flags = throttled_flags();
        let bits: [(u32, &str); 4] = [
            (0, "UNDERVOLTAGE"),
            (1, "FREQ CAPPED"),
            (2, "THROTTLED"),
            (3, "SOFT TEMP LIMIT"),
        ];
        for (bit, name) in bits {
            if flags & (1 << bit) != 0 {
                severe.push(Warning { text: name.to_string(), severe: true });
            } else if flags & (1 << (bit + 16)) != 0 {
                minor.push(Warning { text: format!("{name} SINCE BOOT"), severe: false });
            }
        }

        let (cpu, nvme) = self.temps_now();
        for (label, temp) in [("CPU", cpu), ("NVME", nvme)] {
            let Some(t) = temp else { continue };
            if t >= 85 {
                severe.push(Warning { text: format!("{label} HOT {t}\u{b0}"), severe: true });
            } else if t >= 75 {
                minor.push(Warning { text: format!("{label} WARM {t}\u{b0}"), severe: false });
            }
        }

        let disk = self.disk_percent();
        if disk >= 95 {
            severe.push(Warning { text: format!("DISK {disk}% FULL"), severe: true });
        } else if disk >= 90 {
            minor.push(Warning { text: format!("DISK {disk}% FULL"), severe: false });
        }

        let (used, total) = self.memory_used_total();
        if total > 0 {
            let mem = (used * 100 / total) as u8;
            if mem >= 95 {
                minor.push(Warning { text: format!("RAM {mem}%"), severe: false });
            }
        }

        severe.extend(minor);
        severe
    }
}

fn kernel_hostname() -> String {
    if let Ok(name) = fs::read_to_string("/proc/sys/kernel/hostname") {
        return name.trim().to_string();
    }
    // No procfs (the simulator on a desktop OS) — ask libc instead.
    let mut buf = [0u8; 256];
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0;
    if ok {
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        if len > 0 {
            return String::from_utf8_lossy(&buf[..len]).to_string();
        }
    }
    "unknown".to_string()
}

/// Short hostname: the kernel hostname with any domain part stripped.
pub fn hostname() -> String {
    let name = kernel_hostname();
    name.split('.').next().unwrap_or(&name).to_string()
}

/// Fully qualified hostname: the kernel hostname, extended to an FQDN via
/// /etc/hosts if it's a short name (the usual Proxmox setup).
pub fn fqdn() -> String {
    let short = kernel_hostname();
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
