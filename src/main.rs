//! JetsonTR: Jetson AGX Xavier system monitor on a Thermalright Trofeo Vision LCD.
//!
//! Threads:
//! * collector: samples metrics in three tiers (fast / slow / fixed)
//! * renderer: draws the latest sample once per wall-clock second and caches the JPEG
//! * main: re-sends the cached JPEG every 0.5 s (the panel blanks when frames
//!   stop) and reconnects after errors
//!
//! All settings come from command-line flags; anything not given uses the default.

mod lcd;
mod metrics;
mod render;

use std::process::exit;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use metrics::Intervals;

const RESEND: Duration = Duration::from_millis(500);
const RECONNECT: Duration = Duration::from_secs(5);
const REFRESH: Duration = Duration::from_secs(1);

#[derive(Clone)]
struct Options {
    png: Option<String>,
    intervals: Intervals,
    rotate: bool,
    brightness: f32,
    handshake_only: bool,
    test_secs: Option<u64>,
}

const USAGE: &str = "usage: jetsontr [OPTIONS]

Display:
  --rotate DEG       0 or 180 (default 180)
  --no-rotate        same as --rotate 0
  --brightness F     RGB gain applied before sending (default 1.3)

Metric refresh (seconds; the screen itself redraws every second):
  --fast SECS        CPU, GPU, EMC, power, memory, network, disk I/O (default 1)
  --slow SECS        temperatures, fan, zram, task count (default 5)
  --fixed SECS       power mode (nvpmodel), disk usage (default 60)

Other modes:
  --png PATH         render one frame to PNG and exit (no device needed)
  --handshake-only   query the panel and exit
  --test SECS        show a color test pattern for SECS seconds
  -h, --help         show this help";

fn parse_args() -> Options {
    let mut o = Options {
        png: None,
        intervals: Intervals { fast: Duration::from_secs(1), slow: Duration::from_secs(5), fixed: Duration::from_secs(60) },
        rotate: true,
        brightness: 1.3,
        handshake_only: false,
        test_secs: None,
    };
    let fail = |msg: String| -> ! {
        eprintln!("{}\n\n{}", msg, USAGE);
        exit(2)
    };
    let secs = |flag: &str, v: String| -> Duration {
        match v.parse::<f64>() {
            Ok(s) if (0.5..=3600.0).contains(&s) => Duration::from_secs_f64(s),
            _ => fail(format!("{}: expected seconds between 0.5 and 3600, got {:?}", flag, v)),
        }
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| fail(format!("{} needs a value", a)));
        match a.as_str() {
            "--rotate" => {
                o.rotate = match val().as_str() {
                    "0" => false,
                    "180" => true,
                    v => fail(format!("--rotate: expected 0 or 180, got {:?}", v)),
                }
            }
            "--no-rotate" => o.rotate = false,
            "--brightness" => {
                let v = val();
                o.brightness = match v.parse::<f32>() {
                    Ok(b) if (0.1..=5.0).contains(&b) => b,
                    _ => fail(format!("--brightness: expected 0.1 to 5.0, got {:?}", v)),
                }
            }
            "--fast" | "--interval" => o.intervals.fast = secs(&a, val()),
            "--slow" => o.intervals.slow = secs(&a, val()),
            "--fixed" => o.intervals.fixed = secs(&a, val()),
            "--png" => o.png = Some(val()),
            "--handshake-only" => o.handshake_only = true,
            "--test" => {
                let v = val();
                o.test_secs = Some(v.parse().unwrap_or_else(|_| fail(format!("--test: expected seconds, got {:?}", v))))
            }
            "-h" | "--help" => {
                println!("{}", USAGE);
                exit(0)
            }
            other => fail(format!("unknown option {}", other)),
        }
    }
    o
}

fn connect() -> Result<(lcd::Lcd, lcd::PanelInfo), lcd::LcdError> {
    let dev = lcd::Lcd::open()?;
    let info = dev.handshake()?;
    println!("[OK] LCD PM={} SUB={} FBL={} size={}x{}", info.pm, info.sub, info.fbl, info.width, info.height);
    Ok((dev, info))
}

fn frame_jpeg(pm: &tiny_skia::Pixmap, o: &Options) -> Vec<u8> {
    let rgb = render::to_rgb(pm, o.brightness, o.rotate);
    render::encode_jpeg(&rgb, pm.width(), pm.height())
}

fn main() {
    let o = parse_args();
    let fonts = render::Fonts::load().unwrap_or_else(|e| {
        eprintln!("font: {}", e);
        exit(1)
    });

    if o.handshake_only || o.test_secs.is_some() {
        let (dev, _) = connect().unwrap_or_else(|e| {
            eprintln!("[ERR] {}", e);
            exit(1)
        });
        if let Some(secs) = o.test_secs {
            let start = Instant::now();
            let mut frames = 0;
            while start.elapsed() < Duration::from_secs(secs) {
                let jpeg = frame_jpeg(&render::test_pattern(&fonts, start.elapsed().as_secs() as usize), &o);
                if let Err(e) = dev.send_jpeg(&jpeg) {
                    eprintln!("[ERR] {}", e);
                    exit(1)
                }
                frames += 1;
                thread::sleep(RESEND);
            }
            println!("[OK] sent {} frames", frames);
        }
        return;
    }

    if let Some(path) = &o.png {
        let mut col = metrics::Collector::new(o.intervals);
        let mut snap = None;
        for _ in 0..4 {
            snap = col.sample().or(snap);
            thread::sleep(o.intervals.fast);
        }
        let snap = col.sample().or(snap).expect("no metrics");
        let t0 = Instant::now();
        let pm = render::render(&fonts, &snap);
        let t1 = Instant::now();
        let jpeg = frame_jpeg(&pm, &o);
        let t2 = Instant::now();
        pm.save_png(path).unwrap_or_else(|e| {
            eprintln!("png: {}", e);
            exit(1)
        });
        println!("render {:.0} ms, jpeg {:.0} ms ({} KB) -> {}",
            (t1 - t0).as_secs_f64() * 1e3, (t2 - t1).as_secs_f64() * 1e3, jpeg.len() / 1024, path);
        return;
    }

    let iv = o.intervals;
    println!("[OK] jetsontr rotate={} brightness={} fast={:?} slow={:?} fixed={:?}",
        if o.rotate { 180 } else { 0 }, o.brightness, iv.fast, iv.slow, iv.fixed);

    // Collector thread -> latest snapshot.
    let latest: Arc<Mutex<Option<metrics::Snapshot>>> = Arc::new(Mutex::new(None));
    {
        let latest = latest.clone();
        thread::spawn(move || {
            let mut col = metrics::Collector::new(iv);
            loop {
                let t0 = Instant::now();
                if let Some(s) = col.sample() {
                    *latest.lock().unwrap() = Some(s);
                }
                thread::sleep(iv.fast.saturating_sub(t0.elapsed()));
            }
        });
    }

    // Renderer thread -> cached JPEG, once per wall-clock second.
    let frame: Arc<Mutex<Option<Arc<Vec<u8>>>>> = Arc::new(Mutex::new(None));
    {
        let frame = frame.clone();
        let latest = latest.clone();
        let opts = o.clone();
        thread::spawn(move || loop {
            let snap = latest.lock().unwrap().clone();
            if let Some(mut s) = snap {
                // The clock shows render time, not sample time, so it ticks evenly.
                s.time = chrono::Local::now();
                let jpeg = frame_jpeg(&render::render(&fonts, &s), &opts);
                *frame.lock().unwrap() = Some(Arc::new(jpeg));
            }
            // Wake just after the next second boundary (+20 ms margin).
            let ms = chrono::Timelike::nanosecond(&chrono::Local::now()) / 1_000_000;
            let wait = REFRESH.as_millis() as u64 - (ms.min(999) as u64) + 20;
            thread::sleep(Duration::from_millis(wait));
        });
    }

    // Sender loop with reconnect.
    loop {
        match connect() {
            Ok((dev, _)) => {
                let mut next = Instant::now();
                loop {
                    let jpeg = frame.lock().unwrap().clone();
                    if let Some(j) = jpeg {
                        if let Err(e) = dev.send_jpeg(&j) {
                            eprintln!("[WARN] send: {} - reconnecting in {}s", e, RECONNECT.as_secs());
                            break;
                        }
                    }
                    next += RESEND;
                    let now = Instant::now();
                    if next > now {
                        thread::sleep(next - now);
                    } else {
                        next = now;
                    }
                }
            }
            Err(e) => eprintln!("[WARN] connect: {} - retrying in {}s", e, RECONNECT.as_secs()),
        }
        thread::sleep(RECONNECT);
    }
}
