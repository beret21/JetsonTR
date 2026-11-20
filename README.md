# JetsonTR

**English** | [한국어](README.ko.md)

**JetsonTR** turns a Thermalright Trofeo Vision 9.16" USB LCD (1920×480) into a system monitor for the **NVIDIA Jetson AGX Xavier**.
It drives the display directly over raw USB bulk transfers. You don't need a vendor driver.

It is the Jetson counterpart of [MacTR](https://github.com/beret21/MacTR), the macOS version (TR = Thermalright).

![preview](img/preview.png)

## Hardware

- **Display:** Thermalright Trofeo Vision 9.16 LCD (9.16", 1920×480 IPS, USB Type-C, magnetic mount)
  - Manufacturer: [Black](https://www.thermalright.com/product/trofeo-vision-9-16-lcd-black/) · [White](https://www.thermalright.com/product/trofeo-vision-9-16-lcd-white/)
  - Amazon: [Trofeo Vision 9.16 LCD Black](https://www.amazon.com/dp/B0FX7SH64X)
  - Tested unit: non-ARGB model, USB VID `0x0416` / PID `0x5408`. The ARGB variants were not tested.
- **Host:** NVIDIA Jetson AGX Xavier 16GB (JetPack 5.1.6 / L4T R35.6)

## Panels

| Panel | Content | Source |
|---|---|---|
| CPU | Per-core usage and clock for the 8 Carmel cores (4 clusters × 2), temperature, load average, task count | `/proc/stat`, `cpufreq`, `thermal_zone`, `/proc/loadavg` |
| GPU | Volta GPU load and clock, EMC (memory bandwidth) utilization, temperature, NVENC/NVDEC/VIC activity, network history | `tegrastats`, `17000000.gv11b/devfreq`, `host1x/*/load`, `/proc/net/dev` |
| MEMORY | Unified memory Used / Cache / zram / Available, zram swap I/O history | `/proc/meminfo`, `/sys/block/zram*/mm_stat`, `/proc/vmstat` |
| POWER | Total and per-rail power (W), six temperature sensors, fan RPM and PWM | `tegrastats` (root required), `thermal_zone`, `hwmon` |
| SYSTEM | Clock and date, uptime, NVMe usage, disk I/O history | `/proc/uptime`, `statvfs`, `/proc/diskstats` |

## Requirements

- Tested on Jetson AGX Xavier with JetPack 5.x (L4T R35). Other Jetson models may use different sysfs paths.
- Rust toolchain (stable) and `libusb-1.0` development files to build
- Font: DejaVu Sans (`fonts-dejavu-core`), loaded at runtime

```bash
sudo apt install libusb-1.0-0-dev pkg-config fonts-dejavu-core
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # if Rust is not installed
```

## Build

```bash
cargo build --release
sudo install -m 755 target/release/jetsontr /usr/local/bin/jetsontr
```

The result is a single binary of about 1 MB with no runtime dependencies besides libusb.

## Usage

```bash
# Render one frame to PNG (no device needed)
jetsontr --png img/preview.png

# Query the panel / show a color test pattern for 10 seconds
jetsontr --handshake-only
jetsontr --test 10

# Drive the LCD (power rails are readable only as root)
sudo jetsontr --no-rotate --brightness 1.6
```

| Option | Description |
|---|---|
| `--interval SECS` | Metric collection and render period (default 2) |
| `--no-rotate` | Do not rotate the frame by 180° (pick whichever matches how your LCD is mounted) |
| `--brightness F` | RGB gain applied before sending (default 1.0) |
| `--png PATH` | Save a single PNG and exit, without a device |
| `--handshake-only` | Query the panel and print PM/SUB/FBL |
| `--test SECS` | Show a color test pattern |

### Run as a service (systemd)

```bash
# Allow non-root access (plugdev group)
sudo cp udev/99-thermalright.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger

# Install the service (runs as root so the power rails can be read)
sudo cp systemd/thermalright-lcd.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now thermalright-lcd
journalctl -u thermalright-lcd -f
```

## How it works

- **Threads:** a collector thread and a renderer thread each run every 2 s. The main loop re-sends the cached JPEG every 0.5 s, because the LCD blanks when frames stop arriving.
- **Protocol (LY)**
  - Handshake: send 2048 B, read a 512 B reply, and derive PM/SUB/FBL from it. This unit reports PM=65, FBL=192, 1920×480.
  - Frames: the JPEG (≤ 650 KB) is split into 512 B packets of a 16 B header plus up to 496 B of data. The packet count is the ceiling of size/496, padded to a multiple of 4. Packets go out over bulk OUT (0x09) in transfers of up to 4096 B, then a 512 B ACK is read from bulk IN (0x81).
  - The first handshake after power-up may return `resp[8]=0x02` (not ready), so the handshake is retried.
- **Recovery:** on a send failure or unplug, the device is closed and reopened after 5 s. `tegrastats` is restarted if it exits, and `nvpmodel` is called with a 3 s timeout.
- **Code layout:** `src/lcd.rs` (USB protocol), `src/metrics.rs` (collection), `src/render.rs` (drawing with tiny-skia and ab_glyph, JPEG encoding), `src/main.rs` (threads and CLI).

### Performance (Jetson AGX Xavier, MAXN)

| | Python prototype | Rust |
|---|---|---|
| Render one frame | ~290 ms | ~40 ms |
| CPU (service, one core) | ~13 % | ~3 % |
| Memory (RSS) | ~72 MB | ~13 MB |

## Acknowledgments

- [thermalright-trcc-linux](https://github.com/Lexonight1/thermalright-trcc-linux): reverse engineering of the LY bulk protocol 
- [MacTR](https://github.com/beret21/MacTR): the macOS version, which this screen layout is based on
- Built with [Claude Code](https://claude.com/claude-code). A Mac session and a Jetson session exchanged the protocol notes and the screen spec through cross-session messages.

## License

MIT. See [LICENSE](LICENSE).
The LY protocol details (USB IDs, handshake, packet format) were referenced from [thermalright-trcc-linux](https://github.com/Lexonight1/thermalright-trcc-linux) (GPL-3.0). The implementation is our own: it is written in Rust with its own structure and does not carry over that project's code or its C#-derived quirks. No code from that project is included.
