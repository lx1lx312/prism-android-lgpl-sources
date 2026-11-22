//! Hot-pixel suppression on the sensor mosaic, BEFORE the demosaic (Leo, 07/10/2026: «tanti puntini colorati che
//! Photoshop non ha» — a long night exposure full of isolated red / green / blue dots). Adobe Camera Raw always
//! removes them before demosaicing; a hot photosite that reaches the demosaic is spread over a 3×3 blob of one
//! colour, which is what the picture showed.
//!
//! A photosite is HOT when, at the same time:
//!   1. it is brighter than EVERY photosite of its colour in its 5×5 window by more than `floor` (cheap test, the
//!      only one most photosites ever see: an edge or a gradient always has a same-colour neighbour as bright);
//!   2. it exceeds the median of those neighbours by more than k·σ, σ = 1.4826 · MAD of the same neighbours (and
//!      never by less than `floor`, so a perfectly flat patch with σ ≈ 0 does not turn noise into "hot");
//!   3. it is ISOLATED: the orthogonal neighbours of the OTHER colours do not rise with it. Each one's excess over
//!      its own same-colour median, white-balanced, averages under `kIsolation` of the centre's excess. A real
//!      star — even a tight one, 1.5–3 photosites wide — lights its neighbours of every colour (≥ ~30 % of the
//!      peak at one photosite); a hot photosite lights none. Without at least two such neighbours nothing is
//!      replaced (the test cannot be made).
//! A hot photosite is replaced by the median of its same-colour neighbours. Replacements are collected first and
//! applied afterwards, so a decision never reads a value already changed.
//!
//! Generic over the CFA (Bayer, X-Trans, anything rawler describes with `color_at`): the same-colour and
//! other-colour offsets are tabulated per phase of the pattern. Mono / linear (cpp 3) images are left alone.
//! Threads: row bands over `std::thread::scope`, no new dependency (Windows builds the same).

use rawler::rawimage::{RawImage, RawImageData, RawPhotometricInterpretation};
use std::time::Instant;

/// Default k (in σ) — the middle of Leo's 6–8.
pub const DEFAULT_K: f32 = 7.0;
/// The other-colour neighbours of a hot photosite rise by less than this fraction of its own excess.
const ISOLATION: f32 = 0.15;
/// …and never by less than this fraction of the background above black: shot noise grows with the signal and a
/// 5×5 MAD (8–12 samples, the brightest photosite picked among them) underestimates it — measured on real files
/// (Canon 5D2 ISO 1600, Leica, X-Trans), k·σ alone took 300–2000 ppm of ordinary noise peaks. A hot photosite
/// in a night sky stands several times above its background.
const RELATIVE: f32 = 0.5;
/// The smallest excess that can be "hot", as a fraction of the sensor's range (white − black): ~50 DN on 14 bits.
const FLOOR_OF_RANGE: f32 = 0.003;
/// The window reaches 2 photosites for the same colour and 2 more around the orthogonal neighbours' own medians.
const MARGIN: usize = 3;

#[derive(Default, Clone, Copy)]
pub struct Stats {
    pub replaced: u64,
    pub examined: u64,
    pub millis: f64,
}

/// Offsets of one phase of the pattern.
struct Phase {
    color: usize,
    /// Same-colour photosites of the 5×5 window (centre excluded), as (dy, dx). The first ones are the ±2 / 0
    /// orthogonals when they share the colour, so the early-out of test 1 usually stops after one or two loads.
    same: Vec<(isize, isize)>,
    /// The four orthogonal neighbours at distance 1 whose colour differs.
    others: Vec<(isize, isize)>,
}

fn build_phases(cfa: &rawler::cfa::CFA, ph: usize, pw: usize) -> Vec<Phase> {
    let mut out = Vec::with_capacity(ph * pw);
    for r in 0..ph {
        for c in 0..pw {
            let color = cfa.color_at(r + 48, c + 48);
            let at = |dy: isize, dx: isize| cfa.color_at((r as isize + 48 + dy) as usize, (c as isize + 48 + dx) as usize);
            let mut same: Vec<(isize, isize)> = Vec::new();
            for &(dy, dx) in &[(0isize, -2isize), (0, 2), (-2, 0), (2, 0)] {
                if at(dy, dx) == color {
                    same.push((dy, dx));
                }
            }
            for dy in -2isize..=2 {
                for dx in -2isize..=2 {
                    if (dy == 0 && dx == 0) || same.contains(&(dy, dx)) {
                        continue;
                    }
                    if at(dy, dx) == color {
                        same.push((dy, dx));
                    }
                }
            }
            let mut others = Vec::new();
            for &(dy, dx) in &[(0isize, -1isize), (0, 1), (-1, 0), (1, 0)] {
                if at(dy, dx) != color {
                    others.push((dy, dx));
                }
            }
            out.push(Phase { color, same, others });
        }
    }
    out
}

/// Median of a small slice (sorted in place).
#[inline]
fn median(v: &mut [f32]) -> f32 {
    v.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { 0.5 * (v[n / 2 - 1] + v[n / 2]) }
}

trait Sample: Copy + Send + Sync {
    fn get(self) -> f32;
    fn put(v: f32) -> Self;
}
impl Sample for u16 {
    #[inline]
    fn get(self) -> f32 {
        self as f32
    }
    #[inline]
    fn put(v: f32) -> Self {
        v.round().clamp(0.0, u16::MAX as f32) as u16
    }
}
impl Sample for f32 {
    #[inline]
    fn get(self) -> f32 {
        self
    }
    #[inline]
    fn put(v: f32) -> Self {
        v
    }
}

struct Geometry {
    w: usize,
    x0: usize,
    x1: usize,
    y0: usize,
    y1: usize,
    ph: usize,
    pw: usize,
}

/// Noise bins: two per octave of the signal above black.
const NOISE_BINS: usize = 32;

#[inline]
fn noise_bin(signal: f32) -> usize {
    ((2.0 * (signal.max(0.0) + 1.0).log2()) as usize).min(NOISE_BINS - 1)
}

/// The image's own noise as a function of its level: σ of one photosite per bin, from the difference between a
/// photosite and its first same-colour neighbour on a sparse grid (1 in 45), clipped samples (at black or near
/// white) left out — a black-clipped shadow has a MAD of zero and would make every noise peak "hot". Empty bins
/// take their nearest measured neighbour. Texture inflates the estimate, which only makes the filter more careful.
fn noise_model<T: Sample>(src: &[T], g: &Geometry, phases: &[Phase], black: f32, white: f32) -> [f32; NOISE_BINS] {
    let mut bins: Vec<Vec<f32>> = (0..NOISE_BINS).map(|_| Vec::new()).collect();
    let w = g.w as isize;
    let hi = black + (white - black) * 0.9;
    let mut y = g.y0;
    while y < g.y1 {
        let prow = (y % g.ph) * g.pw;
        let mut x = g.x0;
        while x < g.x1 {
            let p = &phases[prow + x % g.pw];
            if let Some(&(dy, dx)) = p.same.first() {
                let i = (y * g.w + x) as isize;
                let a = src[i as usize].get();
                let b = src[(i + dy * w + dx) as usize].get();
                if a > black + 0.5 && b > black + 0.5 && a < hi && b < hi {
                    bins[noise_bin(0.5 * (a + b) - black)].push((a - b).abs());
                }
            }
            x += 5;
        }
        y += 9;
    }
    let mut sigma = [0.0f32; NOISE_BINS];
    let mut have = [false; NOISE_BINS];
    for (k, v) in bins.iter_mut().enumerate() {
        if v.len() >= 200 {
            let mid = v.len() / 2;
            let (_, m, _) = v.select_nth_unstable_by(mid, |a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            sigma[k] = 1.4826 * *m / std::f32::consts::SQRT_2;
            have[k] = true;
        }
    }
    for k in 0..NOISE_BINS {
        if have[k] {
            continue;
        }
        let mut best: Option<usize> = None;
        for j in 0..NOISE_BINS {
            if have[j] && best.is_none_or(|b| (j as isize - k as isize).abs() < (b as isize - k as isize).abs()) {
                best = Some(j);
            }
        }
        if let Some(b) = best {
            sigma[k] = sigma[b];
        }
    }
    // The noise floor: below the level where the signal stands at twice its own noise, a shadow clipped at black
    // (black-subtracted DNGs clip there) hides half of its noise and the bins read too low — those bins take the
    // first trustworthy one (Leica M11 ISO 3200: 3 DN measured at the bottom, 61 DN where the noise is whole).
    let mut floor_bin = None;
    for k in 0..NOISE_BINS {
        let level = (k as f32 * 0.5).exp2() - 1.0;
        if have[k] && level >= 2.0 * sigma[k] {
            floor_bin = Some(k);
            break;
        }
    }
    if let Some(fb) = floor_bin {
        let s0 = sigma[fb];
        for v in sigma.iter_mut().take(fb) {
            *v = v.max(s0);
        }
    }
    sigma
}

fn run<T: Sample>(data: &mut [T], g: &Geometry, phases: &[Phase], wb: [f32; 4], k: f32, floor: f32, black: f32, white: f32) -> u64 {
    let relative = RELATIVE;
    let model = noise_model(data, g, phases, black, white);
    let model = &model;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 16);
    let rows = g.y1 - g.y0;
    let band = rows.div_ceil(threads).max(64);
    let src: &[T] = data;
    let mut found: Vec<(usize, T)> = Vec::new();
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        let mut y = g.y0;
        while y < g.y1 {
            let ya = y;
            let yb = (y + band).min(g.y1);
            handles.push(scope.spawn(move || {
                let w = g.w as isize;
                let mut hits: Vec<(usize, T)> = Vec::new();
                let mut buf: Vec<f32> = Vec::with_capacity(24);
                let mut dev: Vec<f32> = Vec::with_capacity(24);
                for yy in ya..yb {
                    let prow = (yy % g.ph) * g.pw;
                    let base = (yy * g.w) as isize;
                    for xx in g.x0..g.x1 {
                        let p = &phases[prow + xx % g.pw];
                        let i = base + xx as isize;
                        let v = src[i as usize].get();
                        // 1. brighter than every same-colour neighbour (+ floor): the early-out.
                        let mut brighter = true;
                        for &(dy, dx) in &p.same {
                            if v <= src[(i + dy * w + dx) as usize].get() + floor {
                                brighter = false;
                                break;
                            }
                        }
                        if !brighter || p.same.len() < 4 {
                            continue;
                        }
                        // 2. median + k·σ (σ from the MAD).
                        buf.clear();
                        buf.extend(p.same.iter().map(|&(dy, dx)| src[(i + dy * w + dx) as usize].get()));
                        let med = median(&mut buf);
                        dev.clear();
                        dev.extend(p.same.iter().map(|&(dy, dx)| (src[(i + dy * w + dx) as usize].get() - med).abs()));
                        let sigma = 1.4826 * median(&mut dev);
                        let excess = v - med;
                        let signal = (med - black).max(0.0);
                        let global = model[noise_bin(signal)];
                        if excess <= (k * sigma.max(global)).max(floor).max(relative * signal) {
                            continue;
                        }
                        // 3. isolated: the other colours do not rise with it.
                        if p.others.len() < 2 {
                            continue;
                        }
                        let ex_c = excess * wb[p.color.min(3)];
                        let mut sum = 0.0f32;
                        for &(dy, dx) in &p.others {
                            let q = i + dy * w + dx;
                            let qy = (yy as isize + dy) as usize;
                            let qx = (xx as isize + dx) as usize;
                            let pq = &phases[(qy % g.ph) * g.pw + qx % g.pw];
                            buf.clear();
                            buf.extend(pq.same.iter().map(|&(ey, ex)| src[(q + ey * w + ex) as usize].get()));
                            if buf.is_empty() {
                                continue;
                            }
                            let bg = median(&mut buf);
                            sum += ((src[q as usize].get() - bg) * wb[pq.color.min(3)]).max(0.0);
                        }
                        if sum / (p.others.len() as f32) >= ISOLATION * ex_c {
                            continue;   // a star, a specular highlight: every colour rises
                        }
                        hits.push((i as usize, T::put(med)));
                    }
                }
                hits
            }));
            y = yb;
        }
        for h in handles {
            if let Ok(mut v) = h.join() {
                found.append(&mut v);
            }
        }
    });
    let n = found.len() as u64;
    for (i, v) in found {
        data[i] = v;
    }
    n
}

/// Suppresses the hot photosites of `raw` in place. `None` when the image is not a CFA mosaic (nothing to do).
/// Must run on the raw sensor data: before `apply_scaling` / the demosaic, with the file's white level.
pub fn suppress(raw: &mut RawImage, k: f32) -> Option<Stats> {
    let start = Instant::now();
    if raw.cpp != 1 {
        return None;
    }
    let cfa = match &raw.photometric {
        RawPhotometricInterpretation::Cfa(cfg) => cfg.cfa.clone(),
        _ => return None,
    };
    let (w, h) = (raw.width, raw.height);
    if w < 2 * MARGIN + 1 || h < 2 * MARGIN + 1 {
        return None;
    }
    // The pattern's period: rawler tabulates 48×48; Bayer (2) and X-Trans (6) divide it.
    let ph = if cfa.height > 0 && 48 % cfa.height == 0 { cfa.height } else { 48 };
    let pw = if cfa.width > 0 && 48 % cfa.width == 0 { cfa.width } else { 48 };
    let phases = build_phases(&cfa, ph, pw);
    // A sensor of one colour (or an unknown pattern) has no mosaic to clean.
    let first = phases.first()?.color;
    if phases.iter().all(|p| p.color == first) {
        return None;
    }
    let white = raw.whitelevel.0.first().copied().unwrap_or(u16::MAX as u32) as f32;
    let black = raw.blacklevel.levels.first().map(|r| if r.d != 0 { r.n as f32 / r.d as f32 } else { 0.0 }).unwrap_or(0.0);
    let floor = ((white - black).max(1.0) * FLOOR_OF_RANGE).max(1.0);
    let mut wb = [1.0f32; 4];
    let g_ref = raw.wb_coeffs[1];
    for c in 0..4 {
        let v = raw.wb_coeffs[c];
        if v.is_finite() && v > 0.0 && g_ref.is_finite() && g_ref > 0.0 {
            wb[c] = v / g_ref;
        }
    }
    // The active area (masked borders are black reference, not picture), minus the window's margin INSIDE it: a
    // neighbour in the masked border reads black and would make the last columns look hot.
    let (ax, ay, aw, ah) = match raw.active_area {
        Some(r) => (r.p.x, r.p.y, r.d.w, r.d.h),
        None => (0, 0, w, h),
    };
    let g = Geometry {
        w,
        x0: ax + MARGIN,
        x1: (ax + aw).min(w).saturating_sub(MARGIN),
        y0: ay + MARGIN,
        y1: (ay + ah).min(h).saturating_sub(MARGIN),
        ph,
        pw,
    };
    if g.x1 <= g.x0 || g.y1 <= g.y0 {
        return None;
    }
    let examined = ((g.x1 - g.x0) * (g.y1 - g.y0)) as u64;
    let replaced = match &mut raw.data {
        RawImageData::Integer(v) if v.len() == w * h => run(v.as_mut_slice(), &g, &phases, wb, k, floor, black, white),
        RawImageData::Float(v) if v.len() == w * h => run(v.as_mut_slice(), &g, &phases, wb, k, floor, black, white),
        _ => return None,
    };
    Some(Stats { replaced, examined, millis: start.elapsed().as_secs_f64() * 1000.0 })
}
