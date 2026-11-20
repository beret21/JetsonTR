#!/usr/bin/env python3
"""Jetson AGX Xavier system monitor for the Thermalright Trofeo Vision LCD (1920x480).

Panels: CPU (Carmel clusters) | GPU + accelerators | MEMORY (unified + zram) |
        POWER & THERMAL | SYSTEM + NVMe

Threads:
  collector : reads metrics every --interval seconds (default 2 s), caches a snapshot
  renderer  : draws a frame from the latest snapshot every --interval seconds, caches JPEG
  main      : re-sends the cached JPEG every 0.5 s (LCD blanks if frames stop), reconnects

Power rails (INA3221) are root-only on this board; they come from tegrastats, so run with
sudo to get them. Without root the POWER panel shows temps/fan only.

Usage:
  python3 jetson_lcd_monitor.py --png out.png     # render one frame, no device
  sudo python3 jetson_lcd_monitor.py              # drive the LCD
"""
import argparse
import collections
import datetime
import glob
import io
import os
import re
import subprocess
import sys
import threading
import time

from PIL import Image, ImageDraw, ImageFont

W, H = 1920, 480
SS = 2  # supersampling factor for anti-aliasing

# ---------------------------------------------------------------- colours / fonts
BG_TOP, BG_BOT = (10, 12, 20), (16, 18, 28)
PANEL = (20, 23, 34)
WHITE, SUB, LABEL, DIM = (230, 235, 245), (140, 148, 168), (100, 108, 130), (70, 76, 95)
LINE, TRACK = (38, 42, 58), (30, 34, 48)
GREEN, ORANGE, RED = (52, 211, 153), (251, 191, 36), (239, 68, 68)
GREEN_D, ORANGE_D, RED_D = (24, 95, 70), (110, 84, 16), (110, 30, 30)
BLUE, MAGENTA, LIME, AMBER, CYAN = (66, 133, 244), (217, 70, 239), (52, 211, 153), (251, 191, 36), (34, 211, 238)
TEAL = (45, 212, 191)

FONT_REG = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"
FONT_BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"
_fonts = {}


def font(size, bold=False):
    key = (size, bold)
    if key not in _fonts:
        _fonts[key] = ImageFont.truetype(FONT_BOLD if bold else FONT_REG, int(size * SS))
    return _fonts[key]


def status(pct, lo=50, hi=75):
    if pct < lo:
        return GREEN, GREEN_D
    if pct < hi:
        return ORANGE, ORANGE_D
    return RED, RED_D


def fmt_rate(bps):
    for unit, div in (("GB/s", 1e9), ("MB/s", 1e6), ("KB/s", 1e3)):
        if bps >= div:
            return "%.1f %s" % (bps / div, unit)
    return "%d B/s" % bps


def read(path, default=None):
    try:
        with open(path) as f:
            return f.read().strip()
    except OSError:
        return default


# ---------------------------------------------------------------- metrics
TEGRA_RE = {
    "emc": re.compile(r"EMC_FREQ (\d+)%"),
    "gr3d": re.compile(r"GR3D_FREQ (\d+)%"),
    "rail": re.compile(r"\b([A-Z0-9_]+) (\d+)mW/(\d+)mW"),
}
GPU_DEV = "/sys/devices/platform/17000000.gv11b"
HOST1X = "/sys/devices/platform/13e10000.host1x"
ACCELS = [("ENC", "154c0000.nvenc"), ("ENC1", "15a80000.nvenc1"), ("DEC", "15480000.nvdec"),
          ("DEC1", "15140000.nvdec1"), ("VIC", "15340000.vic")]
NET_SKIP = ("lo", "dummy", "l4tbr", "rndis", "usb", "docker", "veth")
HIST = 60  # samples kept for charts


class Collector(threading.Thread):
    """Reads everything from /proc, /sys and a long-running tegrastats process."""

    def __init__(self, interval):
        super().__init__(daemon=True)
        self.interval = interval
        self.lock = threading.Lock()
        self.snap = None
        self.prev = None
        self.tegra = {}
        self.hist = {k: collections.deque(maxlen=HIST) for k in
                     ("rx", "tx", "dr", "dw", "power", "swin", "swout")}
        self.ncpu = os.cpu_count()
        self.clusters = [read("/sys/devices/system/cpu/cpu%d/topology/physical_package_id" % i, "0")
                         for i in range(self.ncpu)]
        self.cpu_max = int(read("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq", "0")) // 1000
        self.gpu_max = int(read(GPU_DEV + "/devfreq/17000000.gv11b/max_freq", "0")) // 1000000
        self.thermal = {}
        for z in glob.glob("/sys/class/thermal/thermal_zone*"):
            self.thermal[read(z + "/type", "")] = z + "/temp"
        self.nvpmodel = "?"
        self.nvp_t = 0
        self.is_root = os.geteuid() == 0
        threading.Thread(target=self._tegrastats, daemon=True).start()

    # tegrastats runs forever in its own thread; we only parse its latest line
    def _tegrastats(self):
        while True:
            try:
                p = subprocess.Popen(["tegrastats", "--interval", str(int(self.interval * 1000))],
                                     stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
                for line in p.stdout:
                    d = {}
                    m = TEGRA_RE["emc"].search(line)
                    if m:
                        d["emc"] = int(m.group(1))
                    m = TEGRA_RE["gr3d"].search(line)
                    if m:
                        d["gr3d"] = int(m.group(1))
                    rails = {n: int(c) for n, c, _ in TEGRA_RE["rail"].findall(line)}
                    if rails:
                        d["rails"] = rails
                    d["t"] = time.monotonic()
                    self.tegra = d
            except Exception:
                pass
            time.sleep(5)

    def _nvpmodel(self):
        if time.monotonic() - self.nvp_t > 60:
            self.nvp_t = time.monotonic()
            try:
                out = subprocess.run(["nvpmodel", "-q"], capture_output=True, text=True, timeout=3).stdout
                m = re.search(r"NV Power Mode: (\S+)", out)
                if m:
                    self.nvpmodel = m.group(1)
            except Exception:
                pass
        return self.nvpmodel

    @staticmethod
    def _raw():
        r = {"t": time.monotonic()}
        with open("/proc/stat") as f:
            cpus = []
            for line in f:
                if line.startswith("cpu") and line[3].isdigit():
                    v = list(map(int, line.split()[1:]))
                    idle = v[3] + v[4]
                    cpus.append((sum(v[:8]), idle))
                elif line.startswith("procs_running"):
                    r["running"] = int(line.split()[1])
            r["cpu"] = cpus
        rx = tx = 0
        with open("/proc/net/dev") as f:
            for line in f.readlines()[2:]:
                name, data = line.split(":", 1)
                name = name.strip()
                if name.startswith(NET_SKIP):
                    continue
                v = data.split()
                rx += int(v[0])
                tx += int(v[8])
        r["net"] = (rx, tx)
        dr = dw = 0
        with open("/proc/diskstats") as f:
            for line in f:
                v = line.split()
                if v[2] in ("nvme0n1", "mmcblk0"):
                    dr += int(v[5]) * 512
                    dw += int(v[9]) * 512
        r["disk"] = (dr, dw)
        vm = {}
        with open("/proc/vmstat") as f:
            for line in f:
                k, val = line.split()
                if k in ("pswpin", "pswpout"):
                    vm[k] = int(val) * 4096
        r["swap_io"] = (vm.get("pswpin", 0), vm.get("pswpout", 0))
        return r

    def collect(self):
        now = self._raw()
        prev, self.prev = self.prev, now
        if prev is None:
            return
        dt = now["t"] - prev["t"]
        s = {"time": datetime.datetime.now()}

        # CPU
        per = []
        for (t1, i1), (t0, i0) in zip(now["cpu"], prev["cpu"]):
            dtot = t1 - t0
            per.append(100.0 * (1 - (i1 - i0) / dtot) if dtot > 0 else 0.0)
        s["cpu"] = per
        s["cpu_avg"] = sum(per) / len(per)
        s["cpu_freq"] = [int(read("/sys/devices/system/cpu/cpu%d/cpufreq/scaling_cur_freq" % i, "0")) // 1000
                         for i in range(self.ncpu)]
        s["clusters"] = self.clusters
        s["cpu_max"] = self.cpu_max
        s["load"] = read("/proc/loadavg", "0 0 0").split()[:3]
        s["procs"] = len([d for d in os.listdir("/proc") if d.isdigit()])
        s["running"] = now.get("running", 0)

        def temp(name):
            v = read(self.thermal.get(name, ""), None)
            return int(v) / 1000.0 if v else None

        s["temps"] = {k: temp(n) for k, n in (("CPU", "CPU-therm"), ("GPU", "GPU-therm"),
                                              ("SoC", "Tdiode_tegra"), ("Board", "Tboard_tegra"),
                                              ("AUX", "AUX-therm"), ("PMIC", "PMIC-Die"))}

        # GPU + accelerators
        tg = self.tegra if time.monotonic() - self.tegra.get("t", 0) < 3 * self.interval + 2 else {}
        load = read(GPU_DEV + "/load")
        s["gpu"] = tg.get("gr3d", int(load) / 10.0 if load else 0)
        s["gpu_freq"] = int(read(GPU_DEV + "/devfreq/17000000.gv11b/cur_freq", "0")) // 1000000
        s["gpu_max"] = self.gpu_max
        s["emc"] = tg.get("emc")
        acc = []
        for name, d in ACCELS:
            v = read("%s/%s/load" % (HOST1X, d))
            if v is not None:
                acc.append((name, int(v) / 10.0))
        s["accels"] = acc

        # Memory (unified LPDDR4x shared with GPU)
        mi = {}
        with open("/proc/meminfo") as f:
            for line in f:
                k, v = line.split(":")
                mi[k] = int(v.split()[0]) * 1024
        total = mi["MemTotal"]
        avail = mi["MemAvailable"]
        cache = mi["Buffers"] + mi["Cached"] + mi.get("SReclaimable", 0) - mi.get("Shmem", 0)
        s["mem"] = dict(total=total, used=total - avail, avail=avail, cache=max(cache, 0),
                        swap_total=mi["SwapTotal"], swap_used=mi["SwapTotal"] - mi["SwapFree"])
        zr = 0
        for z in glob.glob("/sys/block/zram*/mm_stat"):
            v = read(z, "0 0 0").split()
            zr += int(v[2])  # mem_used_total: physical RAM used by zram
        s["mem"]["zram"] = zr

        # Rates
        rx = (now["net"][0] - prev["net"][0]) / dt
        tx = (now["net"][1] - prev["net"][1]) / dt
        drr = (now["disk"][0] - prev["disk"][0]) / dt
        dww = (now["disk"][1] - prev["disk"][1]) / dt
        swi = (now["swap_io"][0] - prev["swap_io"][0]) / dt
        swo = (now["swap_io"][1] - prev["swap_io"][1]) / dt
        for k, v in (("rx", rx), ("tx", tx), ("dr", drr), ("dw", dww), ("swin", swi), ("swout", swo)):
            self.hist[k].append(max(v, 0))

        # Disk
        st = os.statvfs("/")
        dused = (st.f_blocks - st.f_bfree) * st.f_frsize  # same as df
        dfree = st.f_bavail * st.f_frsize
        s["disk"] = dict(total=dused + dfree, free=dfree, used=dused)

        # Power / fan
        rails = tg.get("rails")
        s["rails"] = rails
        if rails:
            self.hist["power"].append(sum(rails.values()) / 1000.0)
        rpm = read("/sys/class/hwmon/hwmon2/rpm")
        pwm = read("/sys/class/hwmon/hwmon3/pwm1")
        for h in glob.glob("/sys/class/hwmon/hwmon*"):
            n = read(h + "/name")
            if n == "pwm_tach":
                rpm = read(h + "/rpm", rpm)
            elif n == "pwmfan":
                pwm = read(h + "/pwm1", pwm)
        s["fan_rpm"] = int(rpm) if rpm else None
        s["fan_pwm"] = int(pwm) * 100 // 255 if pwm else None
        s["nvpmodel"] = self._nvpmodel()
        s["uptime"] = float(read("/proc/uptime", "0").split()[0])
        s["hist"] = {k: list(v) for k, v in self.hist.items()}
        s["is_root"] = self.is_root
        with self.lock:
            self.snap = s

    def run(self):
        while True:
            t0 = time.monotonic()
            try:
                self.collect()
            except Exception as e:
                print("[WARN] collect:", e, file=sys.stderr)
            time.sleep(max(0.1, self.interval - (time.monotonic() - t0)))

    def latest(self):
        with self.lock:
            return self.snap


# ---------------------------------------------------------------- drawing helpers
class Canvas:
    def __init__(self):
        self.img = Image.new("RGB", (W * SS, H * SS))
        self.d = ImageDraw.Draw(self.img)
        # vertical gradient background
        for y in range(H * SS):
            f = y / (H * SS - 1)
            c = tuple(int(BG_TOP[i] + (BG_BOT[i] - BG_TOP[i]) * f) for i in range(3))
            self.d.line([(0, y), (W * SS, y)], fill=c)

    @staticmethod
    def _s(v):
        return int(round(v * SS))

    def rrect(self, x, y, w, h, r, fill):
        s = self._s
        x0, y0, x1, y1, r = s(x), s(y), s(x + w), s(y + h), s(r)
        r = min(r, (x1 - x0) // 2, (y1 - y0) // 2)
        d = self.d
        if r <= 0:
            d.rectangle([x0, y0, x1 - 1, y1 - 1], fill=fill)
            return
        d.rectangle([x0 + r, y0, x1 - r - 1, y1 - 1], fill=fill)
        d.rectangle([x0, y0 + r, x1 - 1, y1 - r - 1], fill=fill)
        for cx, cy in ((x0, y0), (x1 - 2 * r, y0), (x0, y1 - 2 * r), (x1 - 2 * r, y1 - 2 * r)):
            d.ellipse([cx, cy, cx + 2 * r - 1, cy + 2 * r - 1], fill=fill)

    def text(self, x, y, txt, size, color, bold=False, anchor="l"):
        f = font(size, bold)
        w, _ = self.d.textsize(txt, font=f)
        X = self._s(x)
        if anchor == "r":
            X -= w
        elif anchor == "c":
            X -= w // 2
        self.d.text((X, self._s(y)), txt, font=f, fill=color)
        return w / SS

    def text_w(self, txt, size, bold=False):
        return self.d.textsize(txt, font=font(size, bold))[0] / SS

    def hline(self, x, y, w, color=LINE):
        self.d.rectangle([self._s(x), self._s(y), self._s(x + w), self._s(y) + SS - 1], fill=color)

    def bar(self, x, y, w, h, frac, color, track=TRACK):
        self.rrect(x, y, w, h, h / 2, track)
        frac = max(0.0, min(1.0, frac))
        if frac > 0:
            self.rrect(x, y, max(h, w * frac), h, h / 2, color)

    def arc(self, cx, cy, r, width, start, sweep, color):
        s = self._s
        box = [s(cx - r), s(cy - r), s(cx + r), s(cy + r)]
        if sweep <= 0:
            return
        self.d.arc(box, start, start + sweep, fill=color, width=s(width))
        import math
        rr = r - width / 2
        for a in (start, start + sweep):
            px = cx + rr * math.cos(math.radians(a))
            py = cy + rr * math.sin(math.radians(a))
            self.d.ellipse([s(px - width / 2), s(py - width / 2), s(px + width / 2), s(py + width / 2)],
                           fill=color)

    def gauge(self, cx, cy, pct, color, dark, center=None, unit="%"):
        self.arc(cx, cy, 70, 13, 135, 270, dark)
        self.arc(cx, cy, 70, 13, 135, 270 * max(0, min(pct, 100)) / 100.0, color)
        txt = center if center is not None else "%d" % round(pct)
        self.text(cx, cy - 34, txt, 50 if len(txt) <= 3 else 40, WHITE, bold=True, anchor="c")
        self.text(cx, cy + 24, unit, 20, SUB, anchor="c")

    def mirror_chart(self, x, y, w, h, up, down, up_col, down_col, up_lbl, down_lbl, title):
        self.hline(x, y, w)
        self.text(x, y + 8, title, 17, LABEL, bold=True)
        cy0 = y + 34
        ch = h - 54
        mid = cy0 + ch / 2
        self.text(x + w, y + 8, up_lbl, 15, up_col, anchor="r")
        self.text(x + w, y + h - 18, down_lbl, 15, down_col, anchor="r")
        peak = max([1.0] + list(up) + list(down))
        bw = w / HIST
        half = ch / 2 - 2
        self.hline(x, mid, w, TRACK)
        for series, col, sign in ((up, up_col, -1), (down, down_col, 1)):
            vals = list(series)[-HIST:]
            off = HIST - len(vals)
            colm = tuple(int(c * 0.8 + 20 * 0.2) for c in col)
            for i, v in enumerate(vals):
                bh = half * v / peak
                if bh < 0.5:
                    continue
                bx = x + (off + i) * bw
                if sign < 0:
                    self.rrect(bx + 1, mid - bh, bw - 2, bh, 1, colm)
                else:
                    self.rrect(bx + 1, mid + 1, bw - 2, bh, 1, colm)

    def line_chart(self, x, y, w, h, vals, color, title, right_lbl, vmax=None):
        self.hline(x, y, w)
        self.text(x, y + 8, title, 17, LABEL, bold=True)
        self.text(x + w, y + 8, right_lbl, 15, color, anchor="r")
        top = y + 34
        ch = h - 40
        vals = list(vals)[-HIST:]
        if not vals:
            return
        peak = vmax or max(vals) * 1.2 or 1
        bw = w / HIST
        off = HIST - len(vals)
        colm = tuple(int(c * 0.8 + 20 * 0.2) for c in color)
        for i, v in enumerate(vals):
            bh = max(1, ch * min(v / peak, 1))
            self.rrect(x + (off + i) * bw + 1, top + ch - bh, bw - 2, bh, 1, colm)

    def panel(self, i, title, accent, right=None):
        x = 14 + i * 380
        y = 14
        self.rrect(x, y, 370, 452, 16, PANEL)
        # accent bar + glow, clipped to the rounded top
        self.rrect(x, y, 370, 20, 16, accent)
        self.d.rectangle([self._s(x), self._s(y + 3), self._s(x + 370), self._s(y + 20)], fill=PANEL)
        for k in range(6):
            a = 0.12 * (1 - k / 6)
            c = tuple(int(PANEL[j] * (1 - a) + accent[j] * a) for j in range(3))
            self.hline(x + 6, y + 3 + k, 358, c)
        self.text(x + 20, y + 14, title, 24, accent, bold=True)
        if right:
            self.text(x + 350, y + 20, right, 18, DIM, anchor="r")
        return x, y

    def finish(self):
        return self.img.resize((W, H), Image.LANCZOS)


# ---------------------------------------------------------------- panels
def gb(v):
    return v / 1e9


def draw_cpu(c, s):
    x, y = c.panel(0, "CPU", BLUE, "Carmel x%d" % len(s["cpu"]))
    avg = s["cpu_avg"]
    col, dark = status(avg)
    c.gauge(x + 100, y + 138, avg, col, dark)
    # per-core bars, grouped by cluster
    rx, rw = x + 194, 152
    c.text(rx, y + 60, "Cores  (cluster)", 16, LABEL, bold=True)
    yy = y + 84
    for i, (p, f) in enumerate(zip(s["cpu"], s["cpu_freq"])):
        cl = s["clusters"][i]
        pc, _ = status(p)
        c.text(rx, yy, "C%d" % i, 14, CYAN if int(cl) % 2 else SUB, bold=True)
        c.text(rx + 32, yy, "%.1fG" % (f / 1000.0), 13, DIM)
        c.text(rx + rw, yy, "%d%%" % round(p), 14, WHITE, anchor="r")
        c.bar(rx, yy + 18, rw, 7, p / 100.0, pc)
        yy += 30 if i % 2 else 28
    # bottom
    t = s["temps"]["CPU"]
    tc = RED if t and t > 65 else ORANGE if t and t > 50 else GREEN
    c.hline(x + 20, y + 336, 330)
    c.text(x + 20, y + 350, "Temp", 18, LABEL)
    c.text(x + 80, y + 350, "%.1f°C" % t if t else "N/A", 18, tc, bold=True)
    c.text(x + 350, y + 350, "max %.2f GHz" % (s["cpu_max"] / 1000.0), 16, DIM, anchor="r")
    c.text(x + 20, y + 384, "Load", 18, LABEL)
    c.text(x + 80, y + 384, " / ".join(s["load"]), 18, WHITE)
    c.text(x + 20, y + 418, "Tasks", 18, LABEL)
    c.text(x + 80, y + 418, "%d  (%d running)" % (s["procs"], s["running"]), 18, SUB)


def draw_gpu(c, s):
    x, y = c.panel(1, "GPU", MAGENTA, "Volta 512 CUDA")
    g = s["gpu"]
    c.gauge(x + 100, y + 138, g, MAGENTA, (80, 30, 90))
    rx, rw = x + 194, 152
    c.text(rx, y + 60, "Utilization", 18, LABEL, bold=True)
    rows = [("Freq", "%d MHz" % s["gpu_freq"], s["gpu_freq"] / max(s["gpu_max"], 1), MAGENTA)]
    if s["emc"] is not None:
        rows.append(("EMC", "%d%%" % s["emc"], s["emc"] / 100.0, AMBER))
    t = s["temps"]["GPU"]
    if t:
        rows.append(("Temp", "%.1f°C" % t, t / 100.0, RED if t > 65 else ORANGE if t > 50 else GREEN))
    yy = y + 88
    for lbl, val, frac, col in rows:
        c.text(rx, yy, lbl, 19, SUB)
        c.text(rx + rw, yy, val, 19, WHITE, anchor="r")
        c.bar(rx, yy + 26, rw, 10, frac, col)
        yy += 48
    # accelerators
    c.hline(x + 20, y + 236, 330)
    c.text(x + 20, y + 246, "Engines", 17, LABEL, bold=True)
    ax = x + 20
    for name, v in s["accels"]:
        active = v > 0
        w = 60
        c.rrect(ax, y + 272, w, 22, 11, (110, 40, 125) if active else TRACK)
        c.text(ax + w / 2, y + 275, name, 14, WHITE if active else DIM, bold=True, anchor="c")
        ax += w + 6
    h = s["hist"]
    c.mirror_chart(x + 20, y + 306, 330, 136, h["rx"], h["tx"], CYAN, AMBER,
                   "↓ " + fmt_rate(h["rx"][-1] if h["rx"] else 0),
                   "↑ " + fmt_rate(h["tx"][-1] if h["tx"] else 0), "Network")


def draw_mem(c, s):
    m = s["mem"]
    x, y = c.panel(2, "MEMORY", LIME, "%.1f GB unified" % gb(m["total"]))
    pct = 100.0 * m["used"] / m["total"]
    avail_pct = 100.0 * m["avail"] / m["total"]
    col, dark = (GREEN, GREEN_D) if avail_pct > 25 else (ORANGE, ORANGE_D) if avail_pct > 10 else (RED, RED_D)
    c.gauge(x + 100, y + 138, pct, col, dark)
    rx, rw = x + 194, 152
    c.text(rx, y + 60, "Breakdown", 18, LABEL, bold=True)
    rows = [("Used", m["used"], LIME), ("Cache", m["cache"], BLUE),
            ("zram", m["zram"], MAGENTA), ("Avail", m["avail"], SUB)]
    yy = y + 88
    for lbl, v, colr in rows:
        c.text(rx, yy, lbl, 19, SUB)
        c.text(rx + rw, yy, "%.2f GB" % gb(v), 19, WHITE, anchor="r")
        c.bar(rx, yy + 26, rw, 10, v / m["total"], colr)
        yy += 48
    h = s["hist"]
    act = (h["swin"][-1] if h["swin"] else 0) + (h["swout"][-1] if h["swout"] else 0)
    sc = GREEN if act < 256e3 else ORANGE if act < 10e6 else RED
    c.text(x + 350, y + 306 + 8 - 24, "swap %.2f / %.1f GB" % (gb(m["swap_used"]), gb(m["swap_total"])),
           15, DIM, anchor="r")
    c.mirror_chart(x + 20, y + 306, 330, 136, h["swout"], h["swin"], sc, sc,
                   "out " + fmt_rate(h["swout"][-1] if h["swout"] else 0),
                   "in " + fmt_rate(h["swin"][-1] if h["swin"] else 0), "Swap (zram)")


RAIL_COL = {"GPU": MAGENTA, "CPU": BLUE, "SOC": CYAN, "CV": LIME, "VDDRQ": AMBER, "SYS5V": SUB}


def draw_power(c, s):
    x, y = c.panel(3, "POWER", AMBER, "nvpmodel %s" % s["nvpmodel"])
    rails = s["rails"]
    if rails:
        tot = sum(rails.values()) / 1000.0
        col, dark = status(tot / 30.0 * 100)  # Xavier MAXN budget ~30 W
        c.gauge(x + 100, y + 138, tot / 30.0 * 100, col, dark, center="%.1f" % tot, unit="W")
        rx, rw = x + 194, 152
        c.text(rx, y + 60, "Rails", 18, LABEL, bold=True)
        yy = y + 86
        for name, mw in sorted(rails.items(), key=lambda kv: -kv[1]):
            c.text(rx, yy, name, 15, SUB)
            c.text(rx + rw, yy, "%.2f W" % (mw / 1000.0), 15, WHITE, anchor="r")
            c.bar(rx, yy + 19, rw, 6, mw / max(rails.values()), RAIL_COL.get(name, SUB))
            yy += 32
    else:
        c.text(x + 100, y + 120, "N/A", 40, DIM, bold=True, anchor="c")
        c.text(x + 100, y + 170, "run as root", 16, DIM, anchor="c")
    # temps + fan
    c.hline(x + 20, y + 290, 330)
    temps = s["temps"]
    tx, ty = x + 20, y + 300
    for i, k in enumerate(("CPU", "GPU", "SoC", "Board", "AUX", "PMIC")):
        v = temps.get(k)
        col = (i % 3) * 112
        row = i // 3
        tc = DIM if v is None else RED if v > 65 else ORANGE if v > 50 else GREEN
        if k == "PMIC" and v is not None:
            tc = RED if v > 90 else ORANGE if v > 75 else GREEN
        c.text(tx + col, ty + row * 44, k, 14, LABEL)
        c.text(tx + col, ty + row * 44 + 17, "%.1f°C" % v if v is not None else "--", 18, tc, bold=True)
    fy = y + 396
    c.text(x + 20, fy, "Fan", 18, LABEL)
    rpm, pwm = s["fan_rpm"], s["fan_pwm"]
    c.text(x + 80, fy, "%d rpm" % rpm if rpm is not None else "N/A", 18, WHITE, bold=True)
    if pwm is not None:
        c.text(x + 350, fy, "PWM %d%%" % pwm, 16, SUB, anchor="r")
        c.bar(x + 20, fy + 30, 330, 8, pwm / 100.0, CYAN)


def draw_system(c, s):
    x, y = c.panel(4, "SYSTEM", CYAN, os.uname().nodename)
    t = s["time"]
    c.text(x + 16, y + 48, t.strftime("%H:%M:%S"), 56, WHITE, bold=True)
    c.text(x + 18, y + 120, t.strftime("%Y-%m-%d"), 24, SUB)
    c.text(x + 350, y + 124, t.strftime("%A"), 20, LABEL, anchor="r")
    c.hline(x + 20, y + 160, 330)
    up = int(s["uptime"])
    d, hh, mm = up // 86400, up % 86400 // 3600, up % 3600 // 60
    rows = [("Uptime", ("%dd %dh %dm" % (d, hh, mm)) if d else ("%dh %dm" % (hh, mm))),
            ("Board", "AGX Xavier · L4T R35.6")]
    yy = y + 172
    for k, v in rows:
        c.text(x + 20, yy, k, 18, LABEL)
        c.text(x + 350, yy, v, 18, WHITE, anchor="r")
        yy += 30
    dk = s["disk"]
    pct = 100.0 * dk["used"] / dk["total"]
    col, _ = status(pct, 75, 90)
    c.text(x + 20, yy + 6, "NVMe /", 18, LABEL)
    c.text(x + 350, yy + 6, "%.0f / %.0f GB  (%d%%)" % (gb(dk["used"]), gb(dk["total"]), round(pct)),
           17, WHITE, anchor="r")
    c.bar(x + 20, yy + 34, 330, 10, pct / 100.0, col)
    h = s["hist"]
    c.mirror_chart(x + 20, y + 306, 330, 136, h["dr"], h["dw"], GREEN, ORANGE,
                   "R " + fmt_rate(h["dr"][-1] if h["dr"] else 0),
                   "W " + fmt_rate(h["dw"][-1] if h["dw"] else 0), "Disk I/O")


def render(s):
    c = Canvas()
    for fn in (draw_cpu, draw_gpu, draw_mem, draw_power, draw_system):
        try:
            fn(c, s)
        except Exception as e:
            print("[WARN] %s: %s" % (fn.__name__, e), file=sys.stderr)
    return c.finish()


def encode_jpeg(img, rotate=True, brightness=1.0):
    if brightness != 1.0:
        from PIL import ImageEnhance
        img = ImageEnhance.Brightness(img).enhance(brightness)
    if rotate:
        img = img.rotate(180)
    q = 90
    while True:
        buf = io.BytesIO()
        img.save(buf, "JPEG", quality=q)
        data = buf.getvalue()
        if len(data) <= 650 * 1024 or q <= 30:
            return data
        q -= 5


# ---------------------------------------------------------------- main loops
def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--png", help="render one frame to this PNG and exit (no device)")
    ap.add_argument("--interval", type=float, default=2.0, help="metric/render period (s)")
    ap.add_argument("--no-rotate", action="store_true")
    ap.add_argument("--brightness", type=float, default=1.0,
                    help="RGB gain applied before sending (1.0 = as rendered)")
    a = ap.parse_args()

    col = Collector(a.interval)
    col.start()

    if a.png:
        # need a few samples so rates/charts have data
        deadline = time.monotonic() + a.interval * 4 + 1
        while time.monotonic() < deadline:
            time.sleep(0.2)
        s = col.latest()
        t0 = time.monotonic()
        img = render(s)
        t1 = time.monotonic()
        jpeg = encode_jpeg(img)
        t2 = time.monotonic()
        img.save(a.png)
        print("render %.0f ms, jpeg %.0f ms (%d KB) -> %s" % ((t1 - t0) * 1e3, (t2 - t1) * 1e3,
                                                             len(jpeg) // 1024, a.png))
        return

    import trlcd_test as lcd  # device protocol (handshake / send_frame)
    import usb.util

    frame = {"jpeg": None}

    def renderer():
        while True:
            t0 = time.monotonic()
            s = col.latest()
            if s is not None:
                try:
                    frame["jpeg"] = encode_jpeg(render(s), rotate=not a.no_rotate,
                                                 brightness=a.brightness)
                except Exception as e:
                    print("[WARN] render:", e, file=sys.stderr)
            time.sleep(max(0.1, a.interval - (time.monotonic() - t0)))

    threading.Thread(target=renderer, daemon=True).start()

    while True:
        dev = None
        try:
            dev = lcd.open_device()
            info = lcd.handshake(dev)
            print("[OK] LCD PM=%(pm)d SUB=%(sub)d FBL=%(fbl)d size=%(size)s" % info, flush=True)
            nxt = time.monotonic()
            while True:
                if frame["jpeg"] is not None:
                    lcd.send_frame(dev, frame["jpeg"])
                nxt += 0.5
                wait = nxt - time.monotonic()
                if wait > 0:
                    time.sleep(wait)
                else:
                    nxt = time.monotonic()
        except KeyboardInterrupt:
            break
        except BaseException as e:  # includes sys.exit from open_device when unplugged
            print("[WARN] LCD: %s — reconnecting in 5 s" % e, file=sys.stderr, flush=True)
        finally:
            if dev is not None:
                try:
                    usb.util.release_interface(dev, 0)
                    usb.util.dispose_resources(dev)
                except Exception:
                    pass
        time.sleep(5)


if __name__ == "__main__":
    main()
