//! Metric collection for Jetson AGX Xavier from /proc and /sys.
//!
//! Everything tegrastats reports is read directly (GPU load, EMC activity,
//! INA3221 power rails), so no helper process is needed. The EMC clock and
//! the INA3221 currents are root-only; without root they show as missing.

use std::collections::VecDeque;
use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};


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
    /// This process's CPU use as a share of all cores (%), and its RSS (bytes).
    pub self_cpu: f64,
    pub self_rss: u64,
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

/// One INA3221 channel: label plus voltage (mV) and current (mA) files.
struct Rail {
    label: String,
    mv: String,
    ma: String,
}

fn find_rails() -> Vec<Rail> {
    let mut rails = Vec::new();
    let Ok(devs) = fs::read_dir("/sys/bus/i2c/drivers/ina3221") else { return rails };
    let mut devs: Vec<_> = devs.flatten().map(|d| d.path()).collect();
    devs.sort();
    for dev in devs {
        let Ok(hw) = fs::read_dir(dev.join("hwmon")) else { continue };
        for h in hw.flatten() {
            let h = h.path().display().to_string();
            for ch in 1..=3 {
                if let Some(label) = read(&format!("{}/in{}_label", h, ch)) {
                    rails.push(Rail { label, mv: format!("{}/in{}_input", h, ch), ma: format!("{}/curr{}_input", h, ch) });
                }
            }
        }
    }
    rails
}

/// EMC utilisation in percent: actmon's average memory-controller activity
/// (kHz) over the current EMC clock. Matches tegrastats' EMC_FREQ.
fn emc_util() -> Option<u32> {
    let activity = read_u64("/sys/kernel/actmon_avg_activity/mc_all")?;
    let rate_khz = read_u64("/sys/kernel/debug/clk/emc/clk_rate")? / 1000;
    (rate_khz > 0).then(|| (activity * 100 / rate_khz).min(100) as u32)
}

/// Clock ticks per second for /proc/self/stat (USER_HZ is 100 on Linux/arm64).
const USER_HZ: f64 = 100.0;

fn self_ticks() -> u64 {
    // Fields after the ")" that closes comm: state is field 3, utime 14, stime 15.
    let stat = read("/proc/self/stat").unwrap_or_default();
    let rest = stat.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
    let f: Vec<u64> = rest.split_whitespace().skip(11).take(2).filter_map(|v| v.parse().ok()).collect();
    f.iter().sum()
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
    self_ticks: u64,
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
    Counters { at: Instant::now(), cpu, running, net, disk, swap, self_ticks: self_ticks() }
}

fn self_rss() -> u64 {
    let status = read("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
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

/// How often each group of metrics is refreshed.
#[derive(Clone, Copy)]
pub struct Intervals {
    /// CPU, GPU, EMC, power rails, memory, network, disk I/O.
    pub fast: Duration,
    /// Temperatures, fan, zram, task count.
    pub slow: Duration,
    /// Power mode (nvpmodel) and disk usage.
    pub fixed: Duration,
}

#[derive(Clone, Default)]
struct SlowValues {
    tasks: usize,
    temps: Vec<(&'static str, Option<f64>)>,
    zram: u64,
    fan_rpm: Option<u32>,
    fan_pwm: Option<u32>,
}

#[derive(Clone, Default)]
struct FixedValues {
    power_mode: String,
    disk_total: u64,
    disk_used: u64,
}

fn due(at: Option<Instant>, every: Duration) -> bool {
    // Small slack so a 5 s group sampled by a 1 s loop does not slip to 6 s.
    at.is_none_or(|t| t.elapsed() + Duration::from_millis(100) >= every)
}

pub struct Collector {
    iv: Intervals,
    rails: Vec<Rail>,
    prev: Option<Counters>,
    hist: [VecDeque<f64>; 6],
    cluster: Vec<u32>,
    cpu_max_mhz: u32,
    gpu_max_mhz: u32,
    thermal: Vec<(&'static str, Option<String>)>,
    slow: SlowValues,
    slow_at: Option<Instant>,
    fixed: FixedValues,
    fixed_at: Option<Instant>,
    hostname: String,
}

impl Collector {
    pub fn new(iv: Intervals) -> Collector {
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
            iv,
            rails: find_rails(),
            prev: None,
            hist: Default::default(),
            cluster,
            cpu_max_mhz: (read_u64("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq").unwrap_or(0) / 1000) as u32,
            gpu_max_mhz: (read_u64(&format!("{}/devfreq/17000000.gv11b/max_freq", GPU_DEV)).unwrap_or(0) / 1_000_000) as u32,
            thermal,
            slow: SlowValues::default(),
            slow_at: None,
            fixed: FixedValues { power_mode: "?".into(), ..Default::default() },
            fixed_at: None,
            hostname: read("/proc/sys/kernel/hostname").unwrap_or_default(),
        }
    }

    fn refresh_slow(&mut self) {
        if !due(self.slow_at, self.iv.slow) {
            return;
        }
        self.slow_at = Some(Instant::now());
        let tasks = fs::read_dir("/proc")
            .map(|rd| rd.flatten().filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit())).count())
            .unwrap_or(0);
        let temps = self
            .thermal
            .iter()
            .map(|(label, path)| (*label, path.as_deref().and_then(read_u64).map(|v| v as f64 / 1000.0)))
            .collect();
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
        self.slow = SlowValues {
            tasks,
            temps,
            zram,
            fan_rpm: hwmon_value("pwm_tach", "rpm"),
            fan_pwm: hwmon_value("pwmfan", "pwm1").map(|p| p * 100 / 255),
        };
    }

    fn refresh_fixed(&mut self) {
        if !due(self.fixed_at, self.iv.fixed) {
            return;
        }
        self.fixed_at = Some(Instant::now());
        if let Some(out) = run_with_timeout("nvpmodel", &["-q"], Duration::from_secs(3)) {
            if let Some(mode) = out.lines().find_map(|l| l.strip_prefix("NV Power Mode: ")) {
                self.fixed.power_mode = mode.trim().to_string();
            }
        }
        let (total, used) = statvfs_root();
        self.fixed.disk_total = total;
        self.fixed.disk_used = used;
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

        let gpu = read_u64(&format!("{}/load", GPU_DEV)).map(|v| v as f64 / 10.0).unwrap_or(0.0);
        let emc = emc_util();
        let rails: Vec<(String, u32)> = self
            .rails
            .iter()
            .filter_map(|r| Some((r.label.clone(), (read_u64(&r.mv)? * read_u64(&r.ma)? / 1000) as u32)))
            .collect();
        let ncpu = cpu.len().max(1) as f64;
        let self_cpu = now.self_ticks.saturating_sub(prev.self_ticks) as f64 / USER_HZ / dt * 100.0 / ncpu;
        let self_rss = self_rss();
        let engines = ENGINES
            .iter()
            .filter_map(|(label, dir)| read_u64(&format!("{}/{}/load", HOST1X, dir)).map(|v| (*label, v as f64 / 10.0)))
            .collect();

        let mi = meminfo();
        let g = |k: &str| *mi.get(k).unwrap_or(&0);
        let mem_total = g("MemTotal").max(1);
        let mem_avail = g("MemAvailable");
        let cache = (g("Buffers") + g("Cached") + g("SReclaimable")).saturating_sub(g("Shmem"));

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

        self.refresh_slow();
        self.refresh_fixed();
        let (slow, fixed) = (self.slow.clone(), self.fixed.clone());
        Some(Snapshot {
            hostname: self.hostname.clone(),
            cpu,
            cpu_avg,
            cpu_mhz,
            cluster: self.cluster.clone(),
            cpu_max_mhz: self.cpu_max_mhz,
            load,
            tasks: slow.tasks,
            running,
            temps: slow.temps,
            gpu,
            gpu_mhz: (read_u64(&format!("{}/devfreq/17000000.gv11b/cur_freq", GPU_DEV)).unwrap_or(0) / 1_000_000) as u32,
            gpu_max_mhz: self.gpu_max_mhz,
            emc,
            engines,
            mem_total,
            mem_used: mem_total.saturating_sub(mem_avail),
            mem_avail,
            mem_cache: cache,
            zram: slow.zram,
            swap_total: g("SwapTotal"),
            swap_used: g("SwapTotal").saturating_sub(g("SwapFree")),
            disk_total: fixed.disk_total,
            disk_used: fixed.disk_used,
            rails: if rails.is_empty() { None } else { Some(rails) },
            self_cpu,
            self_rss,
            fan_rpm: slow.fan_rpm,
            fan_pwm: slow.fan_pwm,
            power_mode: fixed.power_mode,
            uptime: read("/proc/uptime").and_then(|s| s.split_whitespace().next()?.parse().ok()).unwrap_or(0.0),
            hist,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_cpu_ticks() {
        let t0 = self_ticks();
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(100) {}
        assert!(self_ticks() > t0);
    }
}
