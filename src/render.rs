//! Draws the 1920x480 monitor frame with tiny-skia and ab_glyph.

use ab_glyph::{point, Font, FontVec, PxScale, ScaleFont};
use tiny_skia::{Color, FillRule, LineCap, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

use crate::metrics::{Snapshot, HISTORY};

pub const W: u32 = 1920;
pub const H: u32 = 480;

type Rgb = (u8, u8, u8);

const BG_TOP: Rgb = (10, 12, 20);
const BG_BOT: Rgb = (16, 18, 28);
const PANEL: Rgb = (20, 23, 34);
const WHITE: Rgb = (230, 235, 245);
const SUB: Rgb = (140, 148, 168);
const LABEL: Rgb = (100, 108, 130);
const DIM: Rgb = (70, 76, 95);
const LINE: Rgb = (38, 42, 58);
const TRACK: Rgb = (30, 34, 48);
const GREEN: Rgb = (52, 211, 153);
const ORANGE: Rgb = (251, 191, 36);
const RED: Rgb = (239, 68, 68);
const GREEN_D: Rgb = (24, 95, 70);
const ORANGE_D: Rgb = (110, 84, 16);
const RED_D: Rgb = (110, 30, 30);
const BLUE: Rgb = (66, 133, 244);
const MAGENTA: Rgb = (217, 70, 239);
const MAGENTA_D: Rgb = (80, 30, 90);
const LIME: Rgb = (52, 211, 153);
const AMBER: Rgb = (251, 191, 36);
const CYAN: Rgb = (34, 211, 238);

const FONT_REG: &str = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf";
const FONT_BOLD: &str = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf";

fn status(pct: f64, lo: f64, hi: f64) -> (Rgb, Rgb) {
    if pct < lo {
        (GREEN, GREEN_D)
    } else if pct < hi {
        (ORANGE, ORANGE_D)
    } else {
        (RED, RED_D)
    }
}

fn temp_color(t: Option<f64>, warn: f64, hot: f64) -> Rgb {
    match t {
        None => DIM,
        Some(v) if v > hot => RED,
        Some(v) if v > warn => ORANGE,
        _ => GREEN,
    }
}

fn fmt_rate(bps: f64) -> String {
    for (unit, div) in [("GB/s", 1e9), ("MB/s", 1e6), ("KB/s", 1e3)] {
        if bps >= div {
            return format!("{:.1} {}", bps / div, unit);
        }
    }
    format!("{} B/s", bps as u64)
}

fn gb(v: u64) -> f64 {
    v as f64 / 1e9
}

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let f = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
    (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

#[derive(Clone, Copy, PartialEq)]
pub enum Align {
    Left,
    Right,
    Center,
}
use Align::*;

pub struct Fonts {
    regular: FontVec,
    bold: FontVec,
}

impl Fonts {
    pub fn load() -> Result<Fonts, String> {
        let load = |p: &str| -> Result<FontVec, String> {
            let data = std::fs::read(p).map_err(|e| format!("{}: {}", p, e))?;
            FontVec::try_from_vec(data).map_err(|e| format!("{}: {}", p, e))
        };
        Ok(Fonts { regular: load(FONT_REG)?, bold: load(FONT_BOLD)? })
    }
}

const PANELS: [(&str, Rgb); 5] = [("CPU", BLUE), ("GPU", MAGENTA), ("MEMORY", LIME), ("POWER", AMBER), ("SYSTEM", CYAN)];

struct Canvas<'a> {
    pm: Pixmap,
    fonts: &'a Fonts,
}

impl<'a> Canvas<'a> {
    fn new(fonts: &'a Fonts) -> Canvas<'a> {
        let mut c = Canvas { pm: Pixmap::new(W, H).unwrap(), fonts };
        for y in 0..H {
            let col = mix(BG_TOP, BG_BOT, y as f64 / (H - 1) as f64);
            c.rect(0.0, y as f32, W as f32, 1.0, col);
        }
        c
    }

    fn paint(col: Rgb) -> Paint<'static> {
        let mut p = Paint::default();
        p.set_color(Color::from_rgba8(col.0, col.1, col.2, 255));
        p.anti_alias = true;
        p
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, col: Rgb) {
        if let Some(r) = Rect::from_xywh(x, y, w.max(0.01), h.max(0.01)) {
            self.pm.fill_rect(r, &Self::paint(col), Transform::identity(), None);
        }
    }

    fn rrect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, col: Rgb) {
        let r = r.min(w / 2.0).min(h / 2.0);
        if r <= 0.5 {
            return self.rect(x, y, w, h, col);
        }
        const K: f32 = 0.552_284_8; // cubic approximation of a quarter circle
        let k = r * K;
        let mut pb = PathBuilder::new();
        pb.move_to(x + r, y);
        pb.line_to(x + w - r, y);
        pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
        pb.line_to(x + w, y + h - r);
        pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
        pb.line_to(x + r, y + h);
        pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
        pb.line_to(x, y + r);
        pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
        pb.close();
        if let Some(path) = pb.finish() {
            self.pm.fill_path(&path, &Self::paint(col), FillRule::Winding, Transform::identity(), None);
        }
    }

    fn hline(&mut self, x: f32, y: f32, w: f32, col: Rgb) {
        self.rect(x, y, w, 1.0, col);
    }

    fn bar(&mut self, x: f32, y: f32, w: f32, h: f32, frac: f64, col: Rgb) {
        self.rrect(x, y, w, h, h / 2.0, TRACK);
        let f = frac.clamp(0.0, 1.0) as f32;
        if f > 0.0 {
            self.rrect(x, y, (w * f).max(h), h, h / 2.0, col);
        }
    }

    fn arc(&mut self, cx: f32, cy: f32, r: f32, width: f32, start: f32, sweep: f32, col: Rgb) {
        if sweep <= 0.0 {
            return;
        }
        let rr = r - width / 2.0;
        let steps = (sweep / 2.0).ceil().max(2.0) as usize;
        let mut pb = PathBuilder::new();
        for i in 0..=steps {
            let a = (start + sweep * i as f32 / steps as f32).to_radians();
            let (px, py) = (cx + rr * a.cos(), cy + rr * a.sin());
            if i == 0 { pb.move_to(px, py) } else { pb.line_to(px, py) }
        }
        if let Some(path) = pb.finish() {
            let stroke = Stroke { width, line_cap: LineCap::Round, ..Default::default() };
            self.pm.stroke_path(&path, &Self::paint(col), &stroke, Transform::identity(), None);
        }
    }

    fn font(&self, bold: bool) -> &'a FontVec {
        if bold { &self.fonts.bold } else { &self.fonts.regular }
    }

    fn scale(font: &FontVec, size: f32) -> PxScale {
        // Treat `size` as the em size in pixels.
        let upem = font.units_per_em().unwrap_or(2048.0);
        PxScale::from(size * font.height_unscaled() / upem)
    }

    fn text_w(&self, s: &str, size: f32, bold: bool) -> f32 {
        let font = self.font(bold);
        let sf = font.as_scaled(Self::scale(font, size));
        let mut w = 0.0;
        let mut prev = None;
        for ch in s.chars() {
            let id = sf.glyph_id(ch);
            if let Some(p) = prev {
                w += sf.kern(p, id);
            }
            w += sf.h_advance(id);
            prev = Some(id);
        }
        w
    }

    /// Draw text with its top edge (ascender line) at `y`.
    fn text(&mut self, x: f32, y: f32, s: &str, size: f32, col: Rgb, bold: bool, align: Align) -> f32 {
        let font = self.font(bold);
        let scale = Self::scale(font, size);
        let sf = font.as_scaled(scale);
        let width = self.text_w(s, size, bold);
        let x0 = match align {
            Left => x,
            Right => x - width,
            Center => x - width / 2.0,
        };
        let baseline = y + sf.ascent();
        let (pw, ph) = (self.pm.width() as i32, self.pm.height() as i32);
        let data = self.pm.data_mut();
        let mut pen = x0;
        let mut prev = None;
        for ch in s.chars() {
            let id = sf.glyph_id(ch);
            if let Some(p) = prev {
                pen += sf.kern(p, id);
            }
            let glyph = id.with_scale_and_position(scale, point(pen, baseline));
            pen += sf.h_advance(id);
            prev = Some(id);
            if let Some(og) = font.outline_glyph(glyph) {
                let b = og.px_bounds();
                og.draw(|gx, gy, cov| {
                    let px = b.min.x as i32 + gx as i32;
                    let py = b.min.y as i32 + gy as i32;
                    if px < 0 || py < 0 || px >= pw || py >= ph {
                        return;
                    }
                    let i = ((py * pw + px) * 4) as usize;
                    let a = cov.clamp(0.0, 1.0);
                    for (k, c) in [col.0, col.1, col.2].into_iter().enumerate() {
                        let d = data[i + k] as f32;
                        data[i + k] = (d + (c as f32 - d) * a).round() as u8;
                    }
                    data[i + 3] = 255;
                });
            }
        }
        width
    }

    fn gauge(&mut self, cx: f32, cy: f32, pct: f64, col: Rgb, dark: Rgb, center: Option<String>, unit: &str) {
        self.arc(cx, cy, 70.0, 13.0, 135.0, 270.0, dark);
        self.arc(cx, cy, 70.0, 13.0, 135.0, 270.0 * pct.clamp(0.0, 100.0) as f32 / 100.0, col);
        let txt = center.unwrap_or_else(|| format!("{}", pct.round() as i64));
        let size = if txt.chars().count() <= 3 { 50.0 } else { 40.0 };
        self.text(cx, cy - 34.0, &txt, size, WHITE, true, Center);
        self.text(cx, cy + 24.0, unit, 20.0, SUB, false, Center);
    }

    #[allow(clippy::too_many_arguments)]
    fn mirror_chart(&mut self, x: f32, y: f32, w: f32, h: f32, up: &[f64], down: &[f64], up_col: Rgb, down_col: Rgb, up_lbl: &str, down_lbl: &str, title: &str) {
        self.hline(x, y, w, LINE);
        self.text(x, y + 8.0, title, 17.0, LABEL, true, Left);
        self.text(x + w, y + 8.0, up_lbl, 15.0, up_col, false, Right);
        self.text(x + w, y + h - 18.0, down_lbl, 15.0, down_col, false, Right);
        let top = y + 34.0;
        let ch = h - 54.0;
        let mid = top + ch / 2.0;
        let half = ch / 2.0 - 2.0;
        let peak = up.iter().chain(down.iter()).fold(1.0f64, |a, &b| a.max(b));
        let bw = w / HISTORY as f32;
        self.hline(x, mid, w, TRACK);
        for (series, col, upward) in [(up, up_col, true), (down, down_col, false)] {
            let col = mix((20, 20, 20), col, 0.8);
            let off = HISTORY.saturating_sub(series.len());
            for (i, &v) in series.iter().rev().take(HISTORY).rev().enumerate() {
                let bh = half * (v / peak) as f32;
                if bh < 0.5 {
                    continue;
                }
                let bx = x + (off + i) as f32 * bw;
                let by = if upward { mid - bh } else { mid + 1.0 };
                self.rrect(bx + 1.0, by, bw - 2.0, bh, 1.0, col);
            }
        }
    }

    fn panel_origin(i: usize) -> (f32, f32) {
        (14.0 + i as f32 * 380.0, 14.0)
    }

    /// Static part of a panel: frame, accent rim and title. Drawn once into the base layer.
    fn panel_frame(&mut self, i: usize) {
        let (title, accent) = PANELS[i];
        let (x, y) = Self::panel_origin(i);
        // accent rim: the panel is drawn 3 px lower over an accent-coloured shape
        self.rrect(x, y, 370.0, 40.0, 16.0, accent);
        self.rrect(x, y + 3.0, 370.0, 449.0, 16.0, PANEL);
        self.rect(x, y + 24.0, 370.0, 20.0, PANEL);
        for k in 0..6 {
            let a = 0.12 * (1.0 - k as f64 / 6.0);
            self.hline(x + 16.0, y + 3.0 + k as f32, 338.0, mix(PANEL, accent, a));
        }
        self.text(x + 20.0, y + 14.0, title, 24.0, accent, true, Left);
    }

    /// Per-frame part of a panel header: the right-hand caption.
    fn panel(&mut self, i: usize, right: &str) -> (f32, f32) {
        let (x, y) = Self::panel_origin(i);
        if !right.is_empty() {
            self.text(x + 350.0, y + 20.0, right, 18.0, DIM, false, Right);
        }
        (x, y)
    }
}

fn last(v: &[f64]) -> f64 {
    v.last().copied().unwrap_or(0.0)
}

fn draw_cpu(c: &mut Canvas, s: &Snapshot) {
    let (x, y) = c.panel(0, &format!("Carmel x{}", s.cpu.len()));
    let (col, dark) = status(s.cpu_avg, 50.0, 75.0);
    c.gauge(x + 100.0, y + 138.0, s.cpu_avg, col, dark, None, "%");
    let (rx, rw) = (x + 194.0, 152.0);
    c.text(rx, y + 60.0, "Cores  (cluster)", 16.0, LABEL, true, Left);
    let mut yy = y + 84.0;
    for (i, (&p, &mhz)) in s.cpu.iter().zip(s.cpu_mhz.iter()).enumerate() {
        let cl = s.cluster.get(i).copied().unwrap_or(0);
        let (pc, _) = status(p, 50.0, 75.0);
        c.text(rx, yy, &format!("C{}", i), 14.0, if cl % 2 == 1 { CYAN } else { SUB }, true, Left);
        c.text(rx + 32.0, yy, &format!("{:.1}G", mhz as f64 / 1000.0), 13.0, DIM, false, Left);
        c.text(rx + rw, yy, &format!("{}%", p.round() as i64), 14.0, WHITE, false, Right);
        c.bar(rx, yy + 18.0, rw, 7.0, p / 100.0, pc);
        yy += if i % 2 == 1 { 30.0 } else { 28.0 };
    }
    let t = s.temp("CPU");
    c.hline(x + 20.0, y + 336.0, 330.0, LINE);
    c.text(x + 20.0, y + 350.0, "Temp", 18.0, LABEL, false, Left);
    let ts = t.map(|v| format!("{:.1}°C", v)).unwrap_or_else(|| "N/A".into());
    c.text(x + 80.0, y + 350.0, &ts, 18.0, temp_color(t, 50.0, 65.0), true, Left);
    c.text(x + 350.0, y + 350.0, &format!("max {:.2} GHz", s.cpu_max_mhz as f64 / 1000.0), 16.0, DIM, false, Right);
    c.text(x + 20.0, y + 384.0, "Load", 18.0, LABEL, false, Left);
    c.text(x + 80.0, y + 384.0, &s.load.join(" / "), 18.0, WHITE, false, Left);
    c.text(x + 20.0, y + 418.0, "Tasks", 18.0, LABEL, false, Left);
    c.text(x + 80.0, y + 418.0, &format!("{}  ({} running)", s.tasks, s.running), 18.0, SUB, false, Left);
}

fn draw_gpu(c: &mut Canvas, s: &Snapshot) {
    let (x, y) = c.panel(1, "Volta 512 CUDA");
    c.gauge(x + 100.0, y + 138.0, s.gpu, MAGENTA, MAGENTA_D, None, "%");
    let (rx, rw) = (x + 194.0, 152.0);
    c.text(rx, y + 60.0, "Utilization", 18.0, LABEL, true, Left);
    let mut rows: Vec<(&str, String, f64, Rgb)> =
        vec![("Freq", format!("{} MHz", s.gpu_mhz), s.gpu_mhz as f64 / s.gpu_max_mhz.max(1) as f64, MAGENTA)];
    if let Some(e) = s.emc {
        rows.push(("EMC", format!("{}%", e), e as f64 / 100.0, AMBER));
    }
    if let Some(t) = s.temp("GPU") {
        rows.push(("Temp", format!("{:.1}°C", t), t / 100.0, temp_color(Some(t), 50.0, 65.0)));
    }
    let mut yy = y + 88.0;
    for (lbl, val, frac, col) in rows {
        c.text(rx, yy, lbl, 19.0, SUB, false, Left);
        c.text(rx + rw, yy, &val, 19.0, WHITE, false, Right);
        c.bar(rx, yy + 26.0, rw, 10.0, frac, col);
        yy += 48.0;
    }
    c.hline(x + 20.0, y + 236.0, 330.0, LINE);
    c.text(x + 20.0, y + 246.0, "Engines", 17.0, LABEL, true, Left);
    let mut ax = x + 20.0;
    for (name, load) in &s.engines {
        let active = *load > 0.0;
        c.rrect(ax, y + 272.0, 60.0, 22.0, 11.0, if active { (110, 40, 125) } else { TRACK });
        c.text(ax + 30.0, y + 275.0, name, 14.0, if active { WHITE } else { DIM }, true, Center);
        ax += 66.0;
    }
    let h = &s.hist;
    c.mirror_chart(x + 20.0, y + 306.0, 330.0, 136.0, &h.rx, &h.tx, CYAN, AMBER,
        &format!("↓ {}", fmt_rate(last(&h.rx))), &format!("↑ {}", fmt_rate(last(&h.tx))), "Network");
}

fn draw_memory(c: &mut Canvas, s: &Snapshot) {
    let (x, y) = c.panel(2, &format!("{:.1} GB unified", gb(s.mem_total)));
    let pct = 100.0 * s.mem_used as f64 / s.mem_total as f64;
    let avail_pct = 100.0 * s.mem_avail as f64 / s.mem_total as f64;
    let (col, dark) = if avail_pct > 25.0 {
        (GREEN, GREEN_D)
    } else if avail_pct > 10.0 {
        (ORANGE, ORANGE_D)
    } else {
        (RED, RED_D)
    };
    c.gauge(x + 100.0, y + 138.0, pct, col, dark, None, "%");
    let (rx, rw) = (x + 194.0, 152.0);
    c.text(rx, y + 60.0, "Breakdown", 18.0, LABEL, true, Left);
    let rows = [("Used", s.mem_used, LIME), ("Cache", s.mem_cache, BLUE), ("zram", s.zram, MAGENTA), ("Avail", s.mem_avail, SUB)];
    let mut yy = y + 88.0;
    for (lbl, v, colr) in rows {
        c.text(rx, yy, lbl, 19.0, SUB, false, Left);
        c.text(rx + rw, yy, &format!("{:.2} GB", gb(v)), 19.0, WHITE, false, Right);
        c.bar(rx, yy + 26.0, rw, 10.0, v as f64 / s.mem_total as f64, colr);
        yy += 48.0;
    }
    let h = &s.hist;
    let act = last(&h.swap_in) + last(&h.swap_out);
    let sc = if act < 256e3 { GREEN } else if act < 10e6 { ORANGE } else { RED };
    c.text(x + 350.0, y + 290.0, &format!("swap {:.2} / {:.1} GB", gb(s.swap_used), gb(s.swap_total)), 15.0, DIM, false, Right);
    c.mirror_chart(x + 20.0, y + 306.0, 330.0, 136.0, &h.swap_out, &h.swap_in, sc, sc,
        &format!("out {}", fmt_rate(last(&h.swap_out))), &format!("in {}", fmt_rate(last(&h.swap_in))), "Swap (zram)");
}

fn rail_color(name: &str) -> Rgb {
    match name {
        "GPU" => MAGENTA,
        "CPU" => BLUE,
        "SOC" => CYAN,
        "CV" => LIME,
        "VDDRQ" => AMBER,
        _ => SUB,
    }
}

fn draw_power(c: &mut Canvas, s: &Snapshot) {
    let (x, y) = c.panel(3, &format!("nvpmodel {}", s.power_mode));
    match &s.rails {
        Some(rails) => {
            let total = rails.iter().map(|(_, mw)| *mw as f64).sum::<f64>() / 1000.0;
            let pct = total / 30.0 * 100.0; // MAXN budget is roughly 30 W
            let (col, dark) = status(pct, 50.0, 75.0);
            c.gauge(x + 100.0, y + 138.0, pct, col, dark, Some(format!("{:.1}", total)), "W");
            let (rx, rw) = (x + 194.0, 152.0);
            c.text(rx, y + 60.0, "Rails", 18.0, LABEL, true, Left);
            let mut sorted = rails.clone();
            sorted.sort_by(|a, b| b.1.cmp(&a.1));
            let peak = sorted.first().map(|r| r.1).unwrap_or(1).max(1) as f64;
            let mut yy = y + 86.0;
            for (name, mw) in sorted.iter().take(6) {
                c.text(rx, yy, name, 15.0, SUB, false, Left);
                c.text(rx + rw, yy, &format!("{:.2} W", *mw as f64 / 1000.0), 15.0, WHITE, false, Right);
                c.bar(rx, yy + 19.0, rw, 6.0, *mw as f64 / peak, rail_color(name));
                yy += 32.0;
            }
        }
        None => {
            c.text(x + 100.0, y + 120.0, "N/A", 40.0, DIM, true, Center);
            c.text(x + 100.0, y + 170.0, "run as root", 16.0, DIM, false, Center);
        }
    }
    c.hline(x + 20.0, y + 290.0, 330.0, LINE);
    for (i, (label, t)) in s.temps.iter().enumerate() {
        let cx = x + 20.0 + (i % 3) as f32 * 112.0;
        let cy = y + 300.0 + (i / 3) as f32 * 44.0;
        let col = if *label == "PMIC" { temp_color(*t, 75.0, 90.0) } else { temp_color(*t, 50.0, 65.0) };
        c.text(cx, cy, label, 14.0, LABEL, false, Left);
        let ts = t.map(|v| format!("{:.1}°C", v)).unwrap_or_else(|| "--".into());
        c.text(cx, cy + 17.0, &ts, 18.0, col, true, Left);
    }
    let fy = y + 396.0;
    c.text(x + 20.0, fy, "Fan", 18.0, LABEL, false, Left);
    let rpm = s.fan_rpm.map(|r| format!("{} rpm", r)).unwrap_or_else(|| "N/A".into());
    c.text(x + 80.0, fy, &rpm, 18.0, WHITE, true, Left);
    if let Some(p) = s.fan_pwm {
        c.text(x + 350.0, fy, &format!("PWM {}%", p), 16.0, SUB, false, Right);
        c.bar(x + 20.0, fy + 30.0, 330.0, 8.0, p as f64 / 100.0, CYAN);
    }
}

fn draw_system(c: &mut Canvas, s: &Snapshot) {
    let (x, y) = c.panel(4, &s.hostname);
    let up = s.uptime as u64;
    let (d, hh, mm) = (up / 86400, up % 86400 / 3600, up % 3600 / 60);
    let uptime = if d > 0 { format!("{}d {}h {}m", d, hh, mm) } else { format!("{}h {}m", hh, mm) };
    c.text(x + 20.0, y + 56.0, "Uptime", 20.0, LABEL, false, Left);
    c.text(x + 18.0, y + 84.0, &uptime, 48.0, WHITE, true, Left);
    c.hline(x + 20.0, y + 160.0, 330.0, LINE);
    let mut yy = y + 172.0;
    let own = format!("{:.2}% CPU · {:.0} MB", s.self_cpu, s.self_rss as f64 / 1048576.0);
    for (k, v) in [("Board", "AGX Xavier · L4T R35.6"), ("JetsonTR", own.as_str())] {
        c.text(x + 20.0, yy, k, 18.0, LABEL, false, Left);
        c.text(x + 350.0, yy, v, 18.0, WHITE, false, Right);
        yy += 30.0;
    }
    let pct = if s.disk_total > 0 { 100.0 * s.disk_used as f64 / s.disk_total as f64 } else { 0.0 };
    let (col, _) = status(pct, 75.0, 90.0);
    c.text(x + 20.0, yy + 6.0, "NVMe /", 18.0, LABEL, false, Left);
    c.text(x + 350.0, yy + 6.0, &format!("{:.0} / {:.0} GB  ({}%)", gb(s.disk_used), gb(s.disk_total), pct.round() as i64), 17.0, WHITE, false, Right);
    c.bar(x + 20.0, yy + 34.0, 330.0, 10.0, pct / 100.0, col);
    let h = &s.hist;
    c.mirror_chart(x + 20.0, y + 306.0, 330.0, 136.0, &h.disk_r, &h.disk_w, GREEN, ORANGE,
        &format!("R {}", fmt_rate(last(&h.disk_r))), &format!("W {}", fmt_rate(last(&h.disk_w))), "Disk I/O");
}

/// Draws frames. The background and panel frames never change, so they are
/// drawn once into `base`; each frame starts from a copy of it.
pub struct Renderer {
    pub fonts: Fonts,
    base: Pixmap,
}

impl Renderer {
    pub fn new(fonts: Fonts) -> Renderer {
        let base = {
            let mut c = Canvas::new(&fonts);
            for i in 0..PANELS.len() {
                c.panel_frame(i);
            }
            c.pm
        };
        Renderer { fonts, base }
    }

    pub fn render(&self, s: &Snapshot) -> Pixmap {
        let mut c = Canvas { pm: self.base.clone(), fonts: &self.fonts };
        draw_cpu(&mut c, s);
        draw_gpu(&mut c, s);
        draw_memory(&mut c, s);
        draw_power(&mut c, s);
        draw_system(&mut c, s);
        c.pm
    }
}

/// Simple color bars for the device test (no metrics needed).
pub fn test_pattern(fonts: &Fonts, phase: usize) -> Pixmap {
    let mut c = Canvas::new(fonts);
    let cols = [RED, GREEN, BLUE, WHITE];
    c.rect(0.0, 0.0, W as f32, H as f32, cols[phase % cols.len()]);
    c.text(W as f32 / 2.0, 200.0, "JetsonTR test pattern", 60.0, (0, 0, 0), true, Center);
    c.pm
}

/// Convert to packed RGB, applying brightness gain and optional 180° rotation.
/// Applies brightness (via a lookup table) and optional 180° rotation, giving
/// RGBX bytes ready for the encoder. Alpha is always 255 here, so it is reused
/// as the padding byte.
pub fn to_rgbx(pm: &Pixmap, brightness: f32, rotate: bool) -> Vec<u8> {
    let mut lut = [0u8; 256];
    for (v, l) in lut.iter_mut().enumerate() {
        *l = (v as f32 * brightness).min(255.0) as u8;
    }
    let src = pm.data();
    let mut out = vec![255u8; src.len()];
    let n = src.len() / 4;
    for (i, d) in out.chunks_exact_mut(4).enumerate() {
        let j = if rotate { n - 1 - i } else { i } * 4;
        d[0] = lut[src[j] as usize];
        d[1] = lut[src[j + 1] as usize];
        d[2] = lut[src[j + 2] as usize];
    }
    out
}

pub fn encode_jpeg(rgbx: &[u8], w: u32, h: u32) -> Vec<u8> {
    let image = turbojpeg::Image { pixels: rgbx, width: w as usize, pitch: w as usize * 4, height: h as usize, format: turbojpeg::PixelFormat::RGBX };
    let mut q = 90;
    loop {
        // The panel's decoder needs 4:2:0 chroma subsampling; 4:4:4 shows as a blank screen.
        let buf = turbojpeg::compress(image, q, turbojpeg::Subsamp::Sub2x2).expect("jpeg encode");
        if buf.len() <= crate::lcd::MAX_JPEG || q <= 30 {
            return buf.to_vec();
        }
        q -= 5;
    }
}
