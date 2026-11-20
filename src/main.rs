//! JetsonTR: Jetson AGX Xavier system monitor on a Thermalright Trofeo Vision LCD.
//!
//! Threads:
//! * collector: samples metrics every `--interval` seconds
//! * renderer: draws the latest sample and caches the JPEG
//! * main: re-sends the cached JPEG every 0.5 s (the panel blanks when frames
//!   stop) and reconnects after errors

mod lcd;
mod metrics;
mod render;

use std::process::exit;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const RESEND: Duration = Duration::from_millis(500);
const RECONNECT: Duration = Duration::from_secs(5);

struct Options {
    png: Option<String>,
    interval: Duration,
    rotate: bool,
    brightness: f32,
    handshake_only: bool,
    test_secs: Option<u64>,
}

const USAGE: &str = "usage: jetsontr [--png PATH] [--interval SECS] [--no-rotate] [--brightness F]
                [--handshake-only] [--test SECS]

  --png PATH        render one frame to PNG and exit (no device needed)
  --interval SECS   metric/render period, default 1
  --no-rotate       do not rotate frames by 180 degrees
  --brightness F    RGB gain applied before sending, default 1.0
  --handshake-only  query the panel and exit
  --test SECS       show a color test pattern for SECS seconds";

fn parse_args() -> Options {
    let mut o = Options { png: None, interval: Duration::from_secs(1), rotate: true, brightness: 1.0, handshake_only: false, test_secs: None };
    let mut args = std::env::args().skip(1);
    let need = |v: Option<String>, flag: &str| -> String {
        v.unwrap_or_else(|| {
            eprintln!("{} needs a value\n{}", flag, USAGE);
            exit(2)
        })
    };
    while let Some(a) = args.next() {
        match a.as_str() {
            "--png" => o.png = Some(need(args.next(), &a)),
            "--interval" => o.interval = Duration::from_secs_f64(need(args.next(), &a).parse().unwrap_or(1.0f64).max(0.5)),
            "--no-rotate" => o.rotate = false,
            "--brightness" => o.brightness = need(args.next(), &a).parse().unwrap_or(1.0),
            "--handshake-only" => o.handshake_only = true,
            "--test" => o.test_secs = Some(need(args.next(), &a).parse().unwrap_or(10)),
            "-h" | "--help" => {
                println!("{}", USAGE);
                exit(0)
            }
            other => {
                eprintln!("unknown option {}\n{}", other, USAGE);
                exit(2)
            }
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
        let mut col = metrics::Collector::new(o.interval);
        let mut snap = None;
        for _ in 0..4 {
            snap = col.sample().or(snap);
            thread::sleep(o.interval);
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

    // Collector thread -> latest snapshot.
    let latest: Arc<Mutex<Option<metrics::Snapshot>>> = Arc::new(Mutex::new(None));
    {
        let latest = latest.clone();
        let interval = o.interval;
        thread::spawn(move || {
            let mut col = metrics::Collector::new(interval);
            loop {
                let t0 = Instant::now();
                if let Some(s) = col.sample() {
                    *latest.lock().unwrap() = Some(s);
                }
                thread::sleep(interval.saturating_sub(t0.elapsed()));
            }
        });
    }

    // Renderer thread -> cached JPEG.
    let frame: Arc<Mutex<Option<Arc<Vec<u8>>>>> = Arc::new(Mutex::new(None));
    {
        let frame = frame.clone();
        let latest = latest.clone();
        let opts = Options { png: None, interval: o.interval, rotate: o.rotate, brightness: o.brightness, handshake_only: false, test_secs: None };
        thread::spawn(move || loop {
            let t0 = Instant::now();
            let snap = latest.lock().unwrap().clone();
            if let Some(mut s) = snap {
                // The clock shows render time, not sample time, so it ticks evenly.
                s.time = chrono::Local::now();
                let jpeg = frame_jpeg(&render::render(&fonts, &s), &opts);
                *frame.lock().unwrap() = Some(Arc::new(jpeg));
            }
            if opts.interval == Duration::from_secs(1) {
                // Align to the next wall-clock second (+20 ms margin).
                let ms = chrono::Timelike::nanosecond(&chrono::Local::now()) / 1_000_000;
                thread::sleep(Duration::from_millis((1000 - ms.min(999) as u64) + 20));
            } else {
                thread::sleep(opts.interval.saturating_sub(t0.elapsed()).max(Duration::from_millis(100)));
            }
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
