//! Metric collection for Jetson AGX Xavier from /proc, /sys and tegrastats.

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};

pub const HISTORY: usize = 60;

const GPU_DEV: &str = "/sys/devices/platform/17000000.gv11b";
const HOST1X: &str = "/sys/devices/platform/13e10000.host1x";
const ENGINES: [(&str, &str); 5] = [
    ("ENC", "154c0000.nvenc"),
    ("ENC1", "15a80000.nvenc1"),
    ("DEC", "15480000.nvdec"),
    ("DEC1", "15140000.nvdec1"),
    ("VIC", "15340000.vic"),
];
const NET_SKIP: [&str; 7] = ["lo", "dummy", "l4tbr", "rndis", "usb", "docker", "veth"];
const DISKS: [&str; 2] = ["nvme0n1", "mmcblk0"];
pub const TEMP_SENSORS: [(&str, &str); 6] = [
    ("CPU", "CPU-therm"),
    ("GPU", "GPU-therm"),
    ("SoC", "Tdiode_tegra"),
    ("Board", "Tboard_tegra"),
    ("AUX", "AUX-therm"),
    ("PMIC", "PMIC-Die"),
];

#[derive(Clone, Default)]
pub struct History {
    pub rx: Vec<f64>,
    pub tx: Vec<f64>,
    pub disk_r: Vec<f64>,
    pub disk_w: Vec<f64>,
    pub swap_in: Vec<f64>,
    pub swap_out: Vec<f64>,
}

#[derive(Clone)]
pub struct Snapshot {
    pub time: DateTime<Local>,
    pub hostname: String,
    pub cpu: Vec<f64>,
    pub cpu_avg: f64,
    pub cpu_mhz: Vec<u32>,
    pub cluster: Vec<u32>,
    pub cpu_max_mhz: u32,
    pub load: [String; 3],
    pub tasks: usize,
    pub running: u64,
    pub temps: Vec<(&'static str, Option<f64>)>,
    pub gpu: f64,
    pub gpu_mhz: u32,
    pub gpu_max_mhz: u32,
    pub emc: Option<u32>,
    pub engines: Vec<(&'static str, f64)>,
    pub mem_total: u64,
    pub mem_used: u64,
    pub mem_avail: u64,
    pub mem_cache: u64,
    pub zram: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub disk_total: u64,
    pub disk_used: u64,
    pub rails: Option<Vec<(String, u32)>>,
    pub fan_rpm: Option<u32>,
    pub fan_pwm: Option<u32>,
    pub power_mode: String,
    pub uptime: f64,
    pub hist: History,
}

impl Snapshot {
    pub fn temp(&self, name: &str) -> Option<f64> {
        self.temps.iter().find(|(n, _)| *n == name).and_then(|(_, v)| *v)
    }
}

fn read(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn read_u64(path: &str) -> Option<u64> {
    read(path).and_then(|s| s.parse().ok())
}

/// Latest values parsed from the tegrastats stream.
#[derive(Clone, Default)]
struct Tegra {
    at: Option<Instant>,
    emc: Option<u32>,
    gr3d: Option<u32>,
    rails: Vec<(String, u32)>,
}

fn parse_tegrastats(line: &str) -> Tegra {
    let toks: Vec<&str> = line.split_whitespace().collect();
    let mut t = Tegra { at: Some(Instant::now()), ..Default::default() };
    let pct = |s: &str| s.split('%').next().and_then(|v| v.parse().ok());
    for (i, tok) in toks.iter().enumerate() {
        let next = toks.get(i + 1).copied().unwrap_or("");
        match *tok {
            "EMC_FREQ" => t.emc = pct(next),
            "GR3D_FREQ" => t.gr3d = pct(next),
            _ => {
                // "<RAIL> <cur>mW/<avg>mW"
                if let Some(cur) = next.split("mW/").next() {
                    if next.ends_with("mW") && next.contains("mW/") && tok.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
                        if let Ok(mw) = cur.parse() {
                            t.rails.push((tok.to_string(), mw));
                        }
                    }
                }
            }
        }
    }
    t
}

fn spawn_tegrastats(interval_ms: u64, slot: Arc<Mutex<Tegra>>) {
    thread::spawn(move || loop {
        if let Ok(mut child) = Command::new("tegrastats")
            .args(["--interval", &interval_ms.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(out) = child.stdout.take() {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    *slot.lock().unwrap() = parse_tegrastats(&line);
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        thread::sleep(Duration::from_secs(5));
    });
}

/// Run a command with a hard timeout; returns stdout on success.
fn run_with_timeout(cmd: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(cmd).args(args).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < timeout => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut s = String::new();
    use std::io::Read;
    child.stdout.take()?.read_to_string(&mut s).ok()?;
    Some(s)
}

struct Counters {
    at: Instant,
    cpu: Vec<(u64, u64)>, // (total, idle)
    running: u64,
    net: (u64, u64),
    disk: (u64, u64),
    swap: (u64, u64),
}

fn read_counters() -> Counters {
    let mut cpu = Vec::new();
    let mut running = 0;
    if let Ok(stat) = fs::read_to_string("/proc/stat") {
        for line in stat.lines() {
            if line.starts_with("cpu") && line.as_bytes().get(3).is_some_and(|b| b.is_ascii_digit()) {
                let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
                if v.len() >= 8 {
                    cpu.push((v[..8].iter().sum(), v[3] + v[4]));
                }
            } else if let Some(r) = line.strip_prefix("procs_running ") {
                running = r.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut net = (0, 0);
    if let Ok(dev) = fs::read_to_string("/proc/net/dev") {
        for line in dev.lines().skip(2) {
            if let Some((name, data)) = line.split_once(':') {
                let name = name.trim();
                if NET_SKIP.iter().any(|p| name.starts_with(p)) {
                    continue;
                }
                let v: Vec<u64> = data.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                if v.len() >= 9 {
                    net.0 += v[0];
                    net.1 += v[8];
                }
            }
        }
    }
    let mut disk = (0, 0);
    if let Ok(ds) = fs::read_to_string("/proc/diskstats") {
        for line in ds.lines() {
            let v: Vec<&str> = line.split_whitespace().collect();
            if v.len() > 9 && DISKS.contains(&v[2]) {
                disk.0 += v[5].parse::<u64>().unwrap_or(0) * 512;
                disk.1 += v[9].parse::<u64>().unwrap_or(0) * 512;
            }
        }
    }
    let mut swap = (0, 0);
    if let Ok(vm) = fs::read_to_string("/proc/vmstat") {
        for line in vm.lines() {
            if let Some((k, v)) = line.split_once(' ') {
                let pages: u64 = v.trim().parse().unwrap_or(0);
                match k {
                    "pswpin" => swap.0 = pages * 4096,
                    "pswpout" => swap.1 = pages * 4096,
                    _ => {}
                }
            }
        }
    }
    Counters { at: Instant::now(), cpu, running, net, disk, swap }
}

fn meminfo() -> std::collections::HashMap<String, u64> {
    let mut m = std::collections::HashMap::new();
    if let Ok(s) = fs::read_to_string("/proc/meminfo") {
        for line in s.lines() {
            if let Some((k, v)) = line.split_once(':') {
                if let Some(kb) = v.split_whitespace().next().and_then(|x| x.parse::<u64>().ok()) {
                    m.insert(k.to_string(), kb * 1024);
                }
            }
        }
    }
    m
}

fn statvfs_root() -> (u64, u64) {
    let path = std::ffi::CString::new("/").unwrap();
    let mut st: libc_statvfs = unsafe { std::mem::zeroed() };
    if unsafe { statvfs(path.as_ptr(), &mut st) } != 0 {
        return (0, 0);
    }
    let frsize = st.f_frsize as u64;
    let used = (st.f_blocks as u64 - st.f_bfree as u64) * frsize; // same as df
    let avail = st.f_bavail as u64 * frsize;
    (used + avail, used)
}

// Minimal statvfs binding (aarch64 Linux / glibc layout) to avoid a libc dependency.
#[repr(C)]
struct libc_statvfs {
    f_bsize: u64,
    f_frsize: u64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_favail: u64,
    f_fsid: u64,
    f_flag: u64,
    f_namemax: u64,
    __spare: [i32; 6],
}
extern "C" {
    fn statvfs(path: *const std::os::raw::c_char, buf: *mut libc_statvfs) -> i32;
}

fn hwmon_value(name: &str, file: &str) -> Option<u32> {
    let dirs = fs::read_dir("/sys/class/hwmon").ok()?;
    for d in dirs.flatten() {
        let p = d.path();
        if read(&format!("{}/name", p.display())).as_deref() == Some(name) {
            return read(&format!("{}/{}", p.display(), file)).and_then(|v| v.parse().ok());
        }
    }
    None
}

pub struct Collector {
    interval: Duration,
    tegra: Arc<Mutex<Tegra>>,
    prev: Option<Counters>,
    hist: [VecDeque<f64>; 6],
    cluster: Vec<u32>,
    cpu_max_mhz: u32,
    gpu_max_mhz: u32,
    thermal: Vec<(&'static str, Option<String>)>,
    power_mode: String,
    power_mode_at: Option<Instant>,
    hostname: String,
}

impl Collector {
    pub fn new(interval: Duration) -> Collector {
        let tegra = Arc::new(Mutex::new(Tegra::default()));
        spawn_tegrastats(interval.as_millis() as u64, tegra.clone());
        let ncpu = (0..).take_while(|i| fs::metadata(format!("/sys/devices/system/cpu/cpu{}", i)).is_ok()).count();
        let cluster = (0..ncpu)
            .map(|i| read_u64(&format!("/sys/devices/system/cpu/cpu{}/topology/physical_package_id", i)).unwrap_or(0) as u32)
            .collect();
        let mut zones = Vec::new();
        if let Ok(rd) = fs::read_dir("/sys/class/thermal") {
            for z in rd.flatten() {
                let p = z.path();
                if let Some(t) = read(&format!("{}/type", p.display())) {
                    zones.push((t, format!("{}/temp", p.display())));
                }
            }
        }
        let thermal = TEMP_SENSORS
            .iter()
            .map(|(label, zone)| (*label, zones.iter().find(|(t, _)| t == zone).map(|(_, p)| p.clone())))
            .collect();
        Collector {
            interval,
            tegra,
            prev: None,
            hist: Default::default(),
            cluster,
            cpu_max_mhz: (read_u64("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq").unwrap_or(0) / 1000) as u32,
            gpu_max_mhz: (read_u64(&format!("{}/devfreq/17000000.gv11b/max_freq", GPU_DEV)).unwrap_or(0) / 1_000_000) as u32,
            thermal,
            power_mode: "?".into(),
            power_mode_at: None,
            hostname: read("/proc/sys/kernel/hostname").unwrap_or_default(),
        }
    }

    fn power_mode(&mut self) -> String {
        if self.power_mode_at.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
            self.power_mode_at = Some(Instant::now());
            if let Some(out) = run_with_timeout("nvpmodel", &["-q"], Duration::from_secs(3)) {
                if let Some(mode) = out.lines().find_map(|l| l.strip_prefix("NV Power Mode: ")) {
                    self.power_mode = mode.trim().to_string();
                }
            }
        }
        self.power_mode.clone()
    }

    /// Take one sample. Returns None on the first call (rates need two samples).
    pub fn sample(&mut self) -> Option<Snapshot> {
        let now = read_counters();
        let prev = self.prev.replace(now);
        let prev = prev?;
        let now = self.prev.as_ref().unwrap();
        let dt = now.at.duration_since(prev.at).as_secs_f64().max(1e-3);

        let cpu: Vec<f64> = now
            .cpu
            .iter()
            .zip(prev.cpu.iter())
            .map(|(&(t1, i1), &(t0, i0))| {
                let dt = t1.saturating_sub(t0) as f64;
                if dt > 0.0 { 100.0 * (1.0 - i1.saturating_sub(i0) as f64 / dt) } else { 0.0 }
            })
            .collect();
        let cpu_avg = if cpu.is_empty() { 0.0 } else { cpu.iter().sum::<f64>() / cpu.len() as f64 };
        let cpu_mhz = (0..cpu.len())
            .map(|i| (read_u64(&format!("/sys/devices/system/cpu/cpu{}/cpufreq/scaling_cur_freq", i)).unwrap_or(0) / 1000) as u32)
            .collect();
        let load_s = read("/proc/loadavg").unwrap_or_default();
        let mut li = load_s.split_whitespace().map(String::from);
        let load = [li.next().unwrap_or_default(), li.next().unwrap_or_default(), li.next().unwrap_or_default()];
        let tasks = fs::read_dir("/proc")
            .map(|rd| rd.flatten().filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit())).count())
            .unwrap_or(0);
        let temps = self
            .thermal
            .iter()
            .map(|(label, path)| (*label, path.as_deref().and_then(read_u64).map(|v| v as f64 / 1000.0)))
            .collect();

        let stale = self.interval * 3 + Duration::from_secs(2);
        let tg = {
            let t = self.tegra.lock().unwrap();
            if t.at.is_some_and(|a| a.elapsed() < stale) { t.clone() } else { Tegra::default() }
        };
        let gpu = tg
            .gr3d
            .map(f64::from)
            .or_else(|| read_u64(&format!("{}/load", GPU_DEV)).map(|v| v as f64 / 10.0))
            .unwrap_or(0.0);
        let engines = ENGINES
            .iter()
            .filter_map(|(label, dir)| read_u64(&format!("{}/{}/load", HOST1X, dir)).map(|v| (*label, v as f64 / 10.0)))
            .collect();

        let mi = meminfo();
        let g = |k: &str| *mi.get(k).unwrap_or(&0);
        let mem_total = g("MemTotal").max(1);
        let mem_avail = g("MemAvailable");
        let cache = (g("Buffers") + g("Cached") + g("SReclaimable")).saturating_sub(g("Shmem"));
        let mut zram = 0;
        if let Ok(rd) = fs::read_dir("/sys/block") {
            for d in rd.flatten() {
                if d.file_name().to_string_lossy().starts_with("zram") {
                    if let Some(s) = read(&format!("{}/mm_stat", d.path().display())) {
                        zram += s.split_whitespace().nth(2).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                    }
                }
            }
        }

        let rate = |a: u64, b: u64| a.saturating_sub(b) as f64 / dt;
        let samples = [
            rate(now.net.0, prev.net.0),
            rate(now.net.1, prev.net.1),
            rate(now.disk.0, prev.disk.0),
            rate(now.disk.1, prev.disk.1),
            rate(now.swap.0, prev.swap.0),
            rate(now.swap.1, prev.swap.1),
        ];
        let running = now.running;
        for (h, v) in self.hist.iter_mut().zip(samples) {
            if h.len() == HISTORY {
                h.pop_front();
            }
            h.push_back(v);
        }
        let v = |i: usize| self.hist[i].iter().copied().collect::<Vec<_>>();
        let hist = History { rx: v(0), tx: v(1), disk_r: v(2), disk_w: v(3), swap_in: v(4), swap_out: v(5) };

        let (disk_total, disk_used) = statvfs_root();
        Some(Snapshot {
            time: Local::now(),
            hostname: self.hostname.clone(),
            cpu,
            cpu_avg,
            cpu_mhz,
            cluster: self.cluster.clone(),
            cpu_max_mhz: self.cpu_max_mhz,
            load,
            tasks,
            running,
            temps,
            gpu,
            gpu_mhz: (read_u64(&format!("{}/devfreq/17000000.gv11b/cur_freq", GPU_DEV)).unwrap_or(0) / 1_000_000) as u32,
            gpu_max_mhz: self.gpu_max_mhz,
            emc: tg.emc,
            engines,
            mem_total,
            mem_used: mem_total.saturating_sub(mem_avail),
            mem_avail,
            mem_cache: cache,
            zram,
            swap_total: g("SwapTotal"),
            swap_used: g("SwapTotal").saturating_sub(g("SwapFree")),
            disk_total,
            disk_used,
            rails: if tg.rails.is_empty() { None } else { Some(tg.rails) },
            fan_rpm: hwmon_value("pwm_tach", "rpm"),
            fan_pwm: hwmon_value("pwmfan", "pwm1").map(|p| p * 100 / 255),
            power_mode: self.power_mode(),
            uptime: read("/proc/uptime").and_then(|s| s.split_whitespace().next()?.parse().ok()).unwrap_or(0.0),
            hist,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tegrastats_line() {
        let l = "11-21-2026 00:21:33 RAM 1092/14887MB (lfb 3012x4MB) SWAP 0/7443MB (cached 0MB) CPU [0%@1190,0%@1188] EMC_FREQ 3%@1331 GR3D_FREQ 12%@[114] VIC_FREQ 1036 APE 150 CPU@34C GPU 0mW/0mW CPU 619mW/619mW SOC 1393mW/1393mW SYS5V 2773mW/2773mW";
        let t = parse_tegrastats(l);
        assert_eq!(t.emc, Some(3));
        assert_eq!(t.gr3d, Some(12));
        assert_eq!(t.rails.len(), 4);
        assert_eq!(t.rails[2], ("SOC".to_string(), 1393));
    }
}
