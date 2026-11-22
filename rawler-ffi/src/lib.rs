//! rawler-ffi — a thin, panic-safe C ABI over `rawler` (LGPL-2.1, the pure-Rust RAW
//! decoder behind DngLab and RapidRAW).
//!
//! Three entry points, all declared in `include/rawler_ffi.h`:
//!   * `rawler_decode_path`    — decode + develop a RAW into linear f32 RGB
//!   * `rawler_thumbnail_jpeg` — pull the largest embedded preview JPEG verbatim
//!   * `rawler_is_supported`   — header sniff
//!
//! plus, in `src/enhance.rs` / `include/rawler_ffi_mosaic.h` ([PR-MAC-172], Prism's Photo ▸ Enhance…), the
//! undeveloped Bayer mosaic (`rawler_mosaic_open` / `_pixels` / `_free`) and a DNG writer for a processed one
//! (`rawler_mosaic_write_dng`, rawler's DngLab conversion).
//!
//! Pixel contract (see the header): interleaved f32 RGB, **linear** (the `SRgb`
//! processing step is removed from rawler's develop pipeline), EXIF orientation
//! physically applied to the buffer, highlights allowed to exceed 1.0 with the
//! CLIPPED CHANNELS — and only those — reconstructed (see `HighlightRecon`), and
//! whatever the camera matrix put outside the sRGB triangle folded back in by
//! desaturation rather than by a per-channel clamp (see `gamut_compress`).
//!
//! Platform-neutral on purpose: no macOS API is touched anywhere in this crate, so
//! the same source builds for Windows.
//!
//! Every `extern "C"` function wraps its body in `catch_unwind` and returns NULL /
//! 0 on failure — a panic inside a decoder (rawler uses `todo!()`/`unreachable!()`
//! liberally for exotic sensors) must never unwind into Swift or C++.

use std::alloc::{Layout, alloc, dealloc};
use std::collections::HashSet;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr;

mod enhance;
mod hotpixels;
mod proxy;

use rawler::decoders::{Orientation, RawDecodeParams};
use rawler::formats::tiff::Rational;
use rawler::imgop::chromatic_adaption::adapt_bradford;
use rawler::imgop::develop::{Intermediate, ProcessingStep, RawDevelop};
use rawler::imgop::matrix::{IDENTITY_MATRIX_3, multiply, normalize, transform_1d};
use rawler::imgop::sensor::bayer::superpixel::Superpixel3Channel;
use rawler::imgop::sensor::{Demosaic, SensorType};
use rawler::imgop::xyz::{Illuminant, SRGB_TO_XYZ_D65};
use rawler::imgop::{Dim2, Point, Rect};
use rawler::pixarray::PixF32;
use rawler::rawimage::{RawImage, RawImageData, RawPhotometricInterpretation};
use rawler::rawsource::RawSource;

// ---------------------------------------------------------------------------
// ABI structs — must match include/rawler_ffi.h field for field.
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct RawlerMeta {
    pub orientation: i32,
    pub iso: f32,
    pub exposure_time: f32,
    pub aperture: f32,
    pub focal_length: f32,
    pub wb_r: f32,
    pub wb_g: f32,
    pub wb_b: f32,
    pub camera_make: [c_char; 64],
    pub camera_model: [c_char; 64],
}

impl Default for RawlerMeta {
    fn default() -> Self {
        Self {
            orientation: 1,
            iso: 0.0,
            exposure_time: 0.0,
            aperture: 0.0,
            focal_length: 0.0,
            wb_r: 1.0,
            wb_g: 1.0,
            wb_b: 1.0,
            camera_make: [0; 64],
            camera_model: [0; 64],
        }
    }
}

#[repr(C)]
pub struct RawlerImage {
    pub width: i32,
    pub height: i32,
    pub channels: i32,
    pub pixels: *mut f32,
    pub meta: RawlerMeta,
}

/// Copy a UTF-8 string into a fixed NUL-terminated C char array, truncating on a
/// char boundary so the result stays valid UTF-8.
fn fill_cstr_field(dst: &mut [c_char; 64], src: &str) {
    let mut end = src.len().min(dst.len() - 1);
    while end > 0 && !src.is_char_boundary(end) {
        end -= 1;
    }
    for (slot, byte) in dst.iter_mut().zip(src.as_bytes()[..end].iter()) {
        *slot = *byte as c_char;
    }
    dst[end] = 0;
}

fn path_from_c(path: *const c_char) -> Option<PathBuf> {
    if path.is_null() {
        return None;
    }
    let s = unsafe { CStr::from_ptr(path) }.to_str().ok()?;
    if s.is_empty() { None } else { Some(PathBuf::from(s)) }
}

fn rational_f32(r: Option<Rational>) -> f32 {
    match r {
        Some(r) if r.d != 0 => r.n as f32 / r.d as f32,
        _ => 0.0,
    }
}

// ---------------------------------------------------------------------------
// Orientation — physically rotate the interleaved RGB buffer.
// ---------------------------------------------------------------------------

/// Rebuild `src` (w×h, 3 f32 per pixel) under `orientation`, returning the new
/// buffer and its post-rotation dimensions. Mirrors `image`'s orientation
/// semantics (the same mapping RapidRAW's `apply_orientation` uses), but works
/// directly on the interleaved buffer so no `image` dependency is needed.
fn apply_orientation(src: Vec<f32>, w: usize, h: usize, orientation: Orientation) -> (Vec<f32>, usize, usize) {
    // (out_w, out_h, fn(out_x, out_y) -> (in_x, in_y))
    type Map = fn(usize, usize, usize, usize) -> (usize, usize);
    let (ow, oh, map): (usize, usize, Map) = match orientation {
        Orientation::Normal | Orientation::Unknown => return (src, w, h),
        // Mirror on the vertical axis.
        Orientation::HorizontalFlip => (w, h, |x, y, w, _h| (w - 1 - x, y)),
        Orientation::Rotate180 => (w, h, |x, y, w, h| (w - 1 - x, h - 1 - y)),
        // Mirror on the horizontal axis.
        Orientation::VerticalFlip => (w, h, |x, y, _w, h| (x, h - 1 - y)),
        // rotate90 + fliph == transpose.
        Orientation::Transpose => (h, w, |x, y, _w, _h| (y, x)),
        // 90° clockwise.
        Orientation::Rotate90 => (h, w, |x, y, _w, h| (y, h - 1 - x)),
        // rotate270 + fliph == anti-transpose.
        Orientation::Transverse => (h, w, |x, y, w, h| (w - 1 - y, h - 1 - x)),
        // 90° counter-clockwise.
        Orientation::Rotate270 => (h, w, |x, y, w, _h| (w - 1 - y, x)),
    };

    let mut out = vec![0.0f32; ow * oh * 3];
    for oy in 0..oh {
        for ox in 0..ow {
            let (ix, iy) = map(ox, oy, w, h);
            let si = (iy * w + ix) * 3;
            let di = (oy * ow + ox) * 3;
            out[di] = src[si];
            out[di + 1] = src[si + 1];
            out[di + 2] = src[si + 2];
        }
    }
    (out, ow, oh)
}

/// 2×2 box downsample of an interleaved RGB buffer (fallback half-size path for
/// sensors the superpixel debayer cannot handle: X-Trans, 4-colour CFAs, …).
fn box_halve(src: &[f32], w: usize, h: usize) -> (Vec<f32>, usize, usize) {
    let (nw, nh) = (w / 2, h / 2);
    if nw == 0 || nh == 0 {
        return (src.to_vec(), w, h);
    }
    let mut out = vec![0.0f32; nw * nh * 3];
    for y in 0..nh {
        for x in 0..nw {
            let (sx, sy) = (x * 2, y * 2);
            let di = (y * nw + x) * 3;
            for c in 0..3 {
                let a = src[(sy * w + sx) * 3 + c];
                let b = src[(sy * w + sx + 1) * 3 + c];
                let d = src[((sy + 1) * w + sx) * 3 + c];
                let e = src[((sy + 1) * w + sx + 1) * 3 + c];
                out[di + c] = (a + b + d + e) * 0.25;
            }
        }
    }
    (out, nw, nh)
}

// ---------------------------------------------------------------------------
// The colour stage: clipping, highlight reconstruction, WB, matrix, gamut
// ---------------------------------------------------------------------------
//
// Why this crate does it and not rawler. Two reasons, both structural:
//
//  * rawler's `Calibrate` ends every pixel with `clip_euclidean_norm_avg`, and
//    the FIRST thing that does is `clip_negative` — the negative sRGB components
//    the camera matrix produces for anything outside the 709 triangle are zeroed
//    inside rawler, unrecoverably. They are exactly what a gamut map needs.
//  * the rest of `clip_euclidean_norm_avg` (the desaturation above 1.0) never
//    fires at all here: the white-level headroom trick in `decode_impl` lifts the
//    white level to u32::MAX, so inside Calibrate every value is ~1e-9. Blown
//    light sources therefore came out magenta — at sensor saturation green pegs
//    first (its white-balance multiplier is the smallest, 1.0 after
//    `normalize_wb`), red and blue keep climbing, and after the WB gains
//    R,B > G — pink. That is the defect `HighlightRecon` was born to fix.
//
// So `ProcessingStep::Calibrate` and `ProcessingStep::WhiteBalance` are dropped
// from the step list (see `LINEAR_STEPS_UNCALIBRATED`) and rawler runs only:
//
//   1. Rescale     n_k = (v_k - black_k) / (white_k - black_k)
//   2. Demosaic
//   3. the crops
//
// With the white level lifted, step 1 divides by (u32::MAX - black) and our
// `rescale` multiplies that back out, so what reaches `develop_pixel` is
// **n_k, the photosite as a fraction of its own full well** — n_k = 1.0 IS the
// sensor's white level. The clip threshold is therefore simply `n_k ≥ 1`, tested
// before any gain is applied, which is the only place the test means anything.
//
// `develop_pixel` then does, in order: clip test → highlight reconstruction →
// white balance → `cam2rgb` (the same matrix Calibrate would have built, see
// `HighlightRecon::cam2rgb`) → `gamut_compress`. Sensors it cannot serve
// (4-colour CFAs, no usable 3×3 matrix) keep `Calibrate` and the historical path.

/// Fraction of the white level at which a channel is treated as clipped. The
/// small margin absorbs per-channel black-level rounding and demosaic ringing.
const HL_CLIP: f32 = 0.995;

/// Where the ramp towards a fully neutral highlight starts, as a fraction of
/// `HL_CLIP`.
///
/// ⚠️ 2026-09-06 (`_MG_4774.CR2`, la fronte bruciata). This was 0.94 — a ramp
/// 0.06 wide, less than a tenth of a stop. A lit forehead crosses it in two or
/// three pixels, so the skin went from its own colour to solid white with NO
/// roll-off: a flat blob with a hard, jagged, aliased rim, which is exactly what
/// Leo photographed. 0.75 spreads the same move over ~0.4 EV, which is a
/// highlight shoulder — the skin desaturates INTO the white instead of stepping
/// into it. Widening it is only safe together with `HL_NEUTRAL_*`: without that
/// gate the wide ramp would wash every bright saturated colour towards white.
const HL_RAMP: f32 = 0.75;

/// The neutrality gate, as `min(c) / max(c)` of the white-balanced pixel — 1 for
/// a grey, 0 for a pure single-channel colour (it is literally 1 − HSV
/// saturation).
///
/// Below `HL_NEUTRAL_LO` no neutral pull is applied at all, above `HL_NEUTRAL_HI`
/// it applies in full, smoothstep in between. This is what tells a blown white
/// light (which should go white) from a saturated emitter (which must not):
/// measured on Leo's two files, the blue LED of `_MG_4863.CR2` sits at ν ≈ 0.013
/// and the lit skin of `_MG_4774.CR2` at ν ≈ 0.54 — two orders of magnitude
/// apart, so the gate is not a delicate threshold.
const HL_NEUTRAL_LO: f32 = 0.06;
const HL_NEUTRAL_HI: f32 = 0.30;

/// smoothstep(0, 1, x) on an already-normalised x.
#[inline]
fn smoothstep01(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// Inverse of a 3×3, or `None` when it is singular. `rgb2cam` is invertible by
/// construction (rawler builds `cam2rgb` as its pseudo-inverse, which for a
/// square matrix IS the inverse), but a broken camera profile must not panic.
fn invert3(m: &[[f32; 3]; 3]) -> Option<[[f32; 3]; 3]> {
    let c00 = m[1][1] * m[2][2] - m[1][2] * m[2][1];
    let c01 = m[1][2] * m[2][0] - m[1][0] * m[2][2];
    let c02 = m[1][0] * m[2][1] - m[1][1] * m[2][0];
    let det = m[0][0] * c00 + m[0][1] * c01 + m[0][2] * c02;
    if !det.is_finite() || det.abs() < 1e-9 {
        return None;
    }
    let d = 1.0 / det;
    let inv = [
        [c00 * d, (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * d, (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * d],
        [c01 * d, (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * d, (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * d],
        [c02 * d, (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * d, (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * d],
    ];
    if inv.iter().flatten().all(|v| v.is_finite()) { Some(inv) } else { None }
}

/// The calibration this crate performs INSTEAD of rawler's `Calibrate` step.
///
/// ⚠️ Why we took the step over (2026-09-06). rawler's `map_3ch_to_rgb` ends
/// every pixel with `clip_euclidean_norm_avg`, whose first act is
/// `clip_negative`: the negative sRGB components the camera matrix produces for
/// any colour outside the 709 triangle are set to 0 *inside rawler*. There is no
/// way to recover them afterwards, and they are precisely the information a
/// gamut map needs. Worse, the reconstruction below has to know how saturated
/// each photosite was, which it can only read off values the matrix has not
/// already mangled. So `ProcessingStep::Calibrate` (and with it `WhiteBalance`,
/// which only exists to feed it) is dropped from the step list and rawler hands
/// us plain black-subtracted, white-level-normalised camera RGB. Everything from
/// there — clip test, highlight reconstruction, white balance, camera matrix,
/// gamut map, in that order — happens in `develop_pixel`.
struct HighlightRecon {
    /// Camera RGB → sRGB-linear: the inverse of
    /// `normalize(xyz2cam · SRGB_TO_XYZ_D65)`, i.e. the very matrix rawler's
    /// Calibrate builds with `pseudo_inverse` (which for a square matrix IS the
    /// inverse).
    ///
    /// `normalize` forces every row of `rgb2cam` to sum to 1.0, so
    /// `rgb2cam · [1,1,1] = [1,1,1]` and therefore `cam2rgb · [L,L,L] = [L,L,L]`
    /// too: a neutral camera triplet stays a neutral sRGB triplet, which is what
    /// makes a reconstructed blown highlight come out grey and not tinted.
    cam2rgb: [[f32; 3]; 3],
    /// The white-balance multipliers. Because Rescale normalises each channel to
    /// 1.0 at its white level, `wb[k]` *is* the white-balanced camera value of a
    /// photosite sitting exactly at saturation.
    wb: [f32; 3],
    /// [Prism 3FR] Where the sensor REALLY saturates, in units of `n` (1.0 = the file's WhiteLevel). 1.0 for every
    /// format but Hasselblad 3FR / FFF, whose WhiteLevel tag (16383 / 32767) is the exposure normalisation Phocus,
    /// ImageIO and Adobe use, not the clip level: the 16-bit data runs to 65535 (4× / 2× the tag) and clips there
    /// (A6008657: 740 photosites at exactly 65535, none between the tag and 60000 piled up). Testing `n ≥ 1` flagged
    /// every sky and cloud above the tag as blown and pulled it towards neutral.
    clip: f32,
}

/// Build the colour transform for `develop_pixel`.
///
/// Returns `None` when it cannot be trusted (no usable 3×3 colour matrix,
/// degenerate white balance, singular matrix) — the caller then leaves
/// `Calibrate` in rawler's hands and gets exactly the old pipeline.
fn highlight_recon(raw: &RawImage, clip: f32) -> Option<HighlightRecon> {
    // Calibrate substitutes [1,1,1,1] when the camera gave no coefficients.
    let coeffs = raw.wb_coeffs;
    let wb = if coeffs[0].is_nan() { [1.0f32; 3] } else { [coeffs[0], coeffs[1], coeffs[2]] };
    if !wb.iter().all(|v| v.is_finite() && *v > 1e-6) {
        return None;
    }

    // Same illuminant priority order and Bradford fallback as Calibrate.
    let xyz2cam: [[f32; 3]; 3] = match raw.color_matrix_find_first([
        Illuminant::D65,
        Illuminant::A,
        Illuminant::B,
        Illuminant::C,
        Illuminant::D50,
        Illuminant::D55,
        Illuminant::D75,
        Illuminant::Daylight,
        Illuminant::Flash,
    ]) {
        Some((Illuminant::D65, m)) => transform_1d::<3, 3>(&m)?,
        // Non-D65 matrices are Bradford-adapted to D65 first, exactly as
        // Calibrate does; a matrix that is not 3×3 makes rawler panic, so bail.
        Some((illu, m)) => adapt_bradford(&illu, &Illuminant::D65, &transform_1d::<3, 3>(&m)?),
        None => IDENTITY_MATRIX_3,
    };

    let rgb2cam = normalize(multiply(&xyz2cam, &SRGB_TO_XYZ_D65));
    if !rgb2cam.iter().flatten().all(|v| v.is_finite()) {
        return None;
    }
    let cam2rgb = invert3(&rgb2cam)?;
    Some(HighlightRecon { cam2rgb, wb, clip: if clip.is_finite() && clip > 1.0 { clip } else { 1.0 } })
}

/// True when `develop_intermediate` will hand back an `Intermediate::ThreeColor`,
/// which is the only shape `develop_pixel` knows how to calibrate. A 4-colour CFA
/// (CYGM / RGBE) needs rawler's own `map_4ch_to_rgb`, so those sensors keep the
/// `Calibrate` step and the historical pipeline.
fn three_color_capable(raw: &RawImage) -> bool {
    match &raw.photometric {
        RawPhotometricInterpretation::Cfa(cfg) => cfg.cfa.is_rgb() && cfg.colors.plane_count() == 3,
        // LinearRaw / BlackIsZero: already interleaved, Demosaic is a no-op.
        _ => raw.cpp == 3,
    }
}

impl HighlightRecon {
    /// One pixel, from black-subtracted sensor RGB to scene-linear sRGB.
    ///
    /// `n[k]` is the photosite as a fraction of its own full well: 1.0 IS the
    /// sensor's white level (rawler's Rescale divides by `white − black`, and
    /// `rescale` undoes the headroom trick). The steps, in the only order that
    /// is physically meaningful:
    ///
    ///   1. test each channel for saturation — **before** white balance, because
    ///      saturation is a property of the photosite, not of the gains;
    ///   2. recover what saturation destroyed, by how blown the pixel is AND how
    ///      neutral it already was;
    ///   3. white balance;
    ///   4. camera matrix;
    ///   5. fold what came out outside the sRGB gamut back in (`gamut_compress`).
    ///
    /// ⚠️ 2026-09-06. Two defects Leo shot on the same EOS 30D turned out to be
    /// the SAME bug — the pixel was dragged to neutral whenever any ONE channel
    /// approached saturation, however coloured it was, and it was dragged there
    /// in less than a tenth of a stop:
    ///
    ///   * `_MG_4863.CR2`, la sveglia blu — a blue LED clips its blue photosite
    ///     with red and green still in the noise, and the old blend hauled red
    ///     and green up too. The cores of the digits measured
    ///     `[2.0469, 2.0469, 2.0469]`, pure white, exactly `wb.b · HL_CLIP`:
    ///     white holes punched in a blue neon.
    ///   * `_MG_4774.CR2`, la fronte bruciata — lit skin, warm, nowhere near
    ///     neutral, blown to solid white the instant green clipped. With the ramp
    ///     0.06 wide that transition took two pixels: a flat blob with a hard,
    ///     jagged rim and no roll-off into the skin at all.
    ///
    /// The pull towards neutral is therefore now weighted by TWO things, and it
    /// still moves the whole pixel together — a per-channel version was tried
    /// first and rings the highlight with saturated contours, because each
    /// channel reaches the ramp at a different scene brightness:
    ///
    ///   * `t`, how close the hottest photosite is to its ceiling — now spread
    ///     over ~0.4 EV (`HL_RAMP`), which is what turns the step into a shoulder;
    ///   * `gate`, how neutral the pixel already is (`HL_NEUTRAL_LO/HI`) — the LED
    ///     is exempt, the skin is not, and a saturated colour that merely happens
    ///     to be bright is exempt too, which is what makes the wide ramp safe.
    ///
    /// A blown NEUTRAL is unaffected by any of it: the pixel is already grey, so
    /// blending it towards its own grey is a no-op. The magenta-highlight fix this
    /// function was written for (green pegs first, red and blue keep climbing,
    /// the blown light goes pink) is the near-neutral case, gate ≈ 1 — it holds.
    #[inline]
    fn develop_pixel(&self, n: [f32; 3]) -> [f32; 3] {
        if self.clip == 1.0 {
            return self.develop_pixel_unit(n);
        }
        // [Prism 3FR] The clip level is `clip`, not 1.0: run the stage with the photosites as a fraction of THEIR
        // saturation, then give the exposure back (every step is homogeneous of degree 1: the matrix, the neutral
        // pull towards the brightest channel, the gamut fold at constant peak).
        let k = 1.0 / self.clip;
        let out = self.develop_pixel_unit([n[0] * k, n[1] * k, n[2] * k]);
        [out[0] * self.clip, out[1] * self.clip, out[2] * self.clip]
    }

    #[inline]
    fn develop_pixel_unit(&self, n: [f32; 3]) -> [f32; 3] {
        // 1 + 2 — clipping, tested in SENSOR space: saturation is a property of
        // the photosite, so the threshold is on `n`, before the white balance.
        let mut c = [n[0] * self.wb[0], n[1] * self.wb[1], n[2] * self.wb[2]];
        let lo = HL_CLIP * HL_RAMP;
        let hottest = n[0].max(n[1]).max(n[2]);
        if hottest > lo {
            // Brightness to reconstruct to: the highest channel that is still
            // believable, i.e. every channel capped at its own clip threshold.
            // For a blown neutral light green is pinned at wb.g (== 1.0) while
            // red and blue still carry signal, so this recovers the true
            // intensity and keeps the >1.0 magnitude the EDR shoulder wants — a
            // fully blown light lands near `max(wb) · HL_CLIP` (≈2× white on a
            // typical daylight WB), which reads as bright neutral in EDR and as
            // plain white once clamped to SDR.
            let level = c[0]
                .min(self.wb[0] * HL_CLIP)
                .max(c[1].min(self.wb[1] * HL_CLIP))
                .max(c[2].min(self.wb[2] * HL_CLIP))
                .max(0.0);

            let t = smoothstep01((hottest - lo) / (HL_CLIP - lo));
            let hi = c[0].max(c[1]).max(c[2]);
            let neutrality = if hi > 1e-6 { c[0].min(c[1]).min(c[2]) / hi } else { 0.0 };
            let gate = smoothstep01((neutrality - HL_NEUTRAL_LO) / (HL_NEUTRAL_HI - HL_NEUTRAL_LO));
            let w = t * gate;
            if w > 0.0 {
                for k in 0..3 {
                    // `max(c[k])`: the pull can raise a channel that lost its
                    // signal, never dim one that still has it — a demosaic
                    // overshoot above the white level must not be pulled back
                    // down, which would hollow out the core of a light.
                    c[k] += w * (level.max(c[k]) - c[k]);
                }
            }
        }

        // 3 + 4 — the white balance is already folded into `c`; apply the matrix.
        let g = &self.cam2rgb;
        let rgb = [
            g[0][0] * c[0] + g[0][1] * c[1] + g[0][2] * c[2],
            g[1][0] * c[0] + g[1][1] * c[1] + g[1][2] * c[2],
            g[2][0] * c[0] + g[2][1] * c[1] + g[2][2] * c[2],
        ];

        // 5 — gamut.
        gamut_compress(rgb)
    }
}

/// Bring a colour the camera can see but sRGB cannot back inside the gamut,
/// WITHOUT flattening its hue.
///
/// The camera matrix routinely produces negative sRGB components: a narrow-band
/// emitter (an LED, a neon, a laser) sits far outside the 709 triangle. Until
/// this crate took the colour stage over, those components were simply zeroed by
/// rawler's own `clip_negative` — the crudest possible gamut map, in which every
/// out-of-gamut blue collapses onto the same `(0, 0, B)` edge. That is why the
/// vectorscope trace sat pinned to the rim and the LED read as one flat
/// posterised blob: measured on `_MG_4863.CR2`, the green channel was EXACTLY
/// 0.0000 across the whole of both digits.
///
/// Instead, DESATURATE AT CONSTANT PEAK: slide the colour along the line towards
/// the neutral `[peak, peak, peak]` — the grey of the same brightest channel —
/// and stop the instant the most negative component reaches exactly 0. The
/// brightest channel is a fixed point of that line, so nothing dims; the chroma
/// is reduced by the smallest amount that makes the colour representable at all;
/// and because the amount scales with how far out the colour is, a light that
/// gets progressively more saturated towards its core keeps a GRADIENT instead of
/// landing on one flat edge value. A pixel already inside the gamut returns
/// untouched, bit for bit, so nothing an ordinary picture contains is affected.
///
/// ⚠️ NOT the 709 luminance as the anchor, tempting as it is. Luminance is
/// computed with a big positive weight on green, so for the very colours this
/// function exists for — a blue LED whose green component is strongly negative —
/// it collapses: measured on `_MG_4863.CR2`, a pixel at `[0.078, −0.248, 2.536]`
/// has a 709 luminance of 0.022, and "preserving" that turns the brightest thing
/// in the frame into `[0.027, 0, 0.230]`, a near-black smudge. The peak channel is
/// the only anchor that is guaranteed both positive and bright.
#[inline]
fn gamut_compress(rgb: [f32; 3]) -> [f32; 3] {
    let lowest = rgb[0].min(rgb[1]).min(rgb[2]);
    if lowest >= 0.0 {
        return rgb;
    }
    let peak = rgb[0].max(rgb[1]).max(rgb[2]);
    if !(peak > 0.0) {
        return [0.0, 0.0, 0.0]; // nothing but noise below black
    }
    // peak + s·(v − peak) ≥ 0 ⇒ s ≤ peak / (peak − v), for every negative v.
    let mut s = 1.0f32;
    for v in rgb {
        if v < 0.0 {
            s = s.min(peak / (peak - v));
        }
    }
    let s = s.clamp(0.0, 1.0);
    [
        (peak + s * (rgb[0] - peak)).max(0.0),
        (peak + s * (rgb[1] - peak)).max(0.0),
        (peak + s * (rgb[2] - peak)).max(0.0),
    ]
}

// ---------------------------------------------------------------------------
// Develop
// ---------------------------------------------------------------------------

/// Everything but `SRgb`: we want scene-linear output, the tone curve belongs to
/// the caller's editing shader.
///
/// `WhiteBalance` + `Calibrate` are here only for the sensors `HighlightRecon`
/// cannot serve (4-colour CFAs, cameras with no usable 3×3 matrix). Everything
/// else takes `LINEAR_STEPS_UNCALIBRATED` and gets its colour from
/// `HighlightRecon::develop_pixel` — see that struct's header for why.
const LINEAR_STEPS: [ProcessingStep; 7] = [
    ProcessingStep::Rescale,
    ProcessingStep::Demosaic,
    ProcessingStep::FujiRotate,
    ProcessingStep::CropActiveArea,
    ProcessingStep::WhiteBalance,
    ProcessingStep::Calibrate,
    ProcessingStep::CropDefault,
];

/// `LINEAR_STEPS` minus the colour stages: rawler stops after the demosaic and
/// the crop, and hands back black-subtracted, white-level-normalised CAMERA RGB.
const LINEAR_STEPS_UNCALIBRATED: [ProcessingStep; 5] = [
    ProcessingStep::Rescale,
    ProcessingStep::Demosaic,
    ProcessingStep::FujiRotate,
    ProcessingStep::CropActiveArea,
    ProcessingStep::CropDefault,
];

/// The half-size path pre-debayers with superpixel and hands rawler an already
/// 3-channel image, so `Rescale`/`Demosaic` must not run again.
const LINEAR_STEPS_PREDEBAYERED: [ProcessingStep; 3] =
    [ProcessingStep::WhiteBalance, ProcessingStep::Calibrate, ProcessingStep::CropDefault];

/// Its uncalibrated twin (see `LINEAR_STEPS_UNCALIBRATED`).
const LINEAR_STEPS_PREDEBAYERED_UNCALIBRATED: [ProcessingStep; 1] = [ProcessingStep::CropDefault];

/// Flatten a developed intermediate into interleaved RGB, scaling by `rescale`.
///
/// `rescale` undoes the white-level headroom trick below, so a pixel at the
/// sensor's white level lands on 1.0 and genuine highlights land above it.
///
/// `recon`, when present, means rawler was run WITHOUT `WhiteBalance`/`Calibrate`
/// and this function owns the whole colour stage — clipping, highlight
/// reconstruction, white balance, camera matrix and gamut map, in that order (see
/// `HighlightRecon::develop_pixel`). When it is `None` rawler calibrated the
/// pixels itself and they only need scaling, exactly as before.
/// Scale a rawler value into "1.0 == the sensor's own white level" and drop
/// NaN/inf; the ceiling stays at 64× so the EDR shoulder keeps its headroom.
/// A free function rather than a closure so the threaded arm below can call it
/// without capturing anything.
#[inline]
fn clean_scaled(v: f32, rescale: f32) -> f32 {
    let v = v * rescale;
    if v.is_finite() { v.clamp(0.0, 64.0) } else { 0.0 }
}

fn intermediate_to_rgb(
    inter: Intermediate,
    rescale: f32,
    recon: Option<&HighlightRecon>,
) -> Option<(Vec<f32>, usize, usize)> {
    let dim = inter.dim();
    let (w, h) = (dim.w, dim.h);
    if w == 0 || h == 0 {
        return None;
    }
    let clean = |v: f32| clean_scaled(v, rescale);
    let mut out = vec![0.0f32; w * h * 3];
    match inter {
        Intermediate::ThreeColor(pixels) => match recon {
            // The colour stage is ours, and it is the expensive arm: a 3×3
            // matrix plus the highlight and gamut work on every pixel. rawler
            // ran its own Calibrate under rayon, so doing this serially made a
            // full-resolution decode 35 % slower (measured, 10 MP Fuji:
            // 1.01 s → 1.36 s). Split by row bands over plain scoped threads —
            // no new dependency, and it builds for Windows like the rest.
            Some(r) => {
                let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 16);
                // At least 32 rows per band: below that the spawn costs more
                // than the work, and a thumbnail-sized decode stays serial.
                let band_rows = h.div_ceil(threads).max(32);
                let band = band_rows * w;
                std::thread::scope(|scope| {
                    for (dst, src) in out.chunks_mut(band * 3).zip(pixels.data.chunks(band)) {
                        scope.spawn(move || {
                            for (d, p) in dst.chunks_exact_mut(3).zip(src.iter()) {
                                let px = r.develop_pixel([
                                    clean_scaled(p[0], rescale),
                                    clean_scaled(p[1], rescale),
                                    clean_scaled(p[2], rescale),
                                ]);
                                d[0] = px[0];
                                d[1] = px[1];
                                d[2] = px[2];
                            }
                        });
                    }
                });
            }
            // rawler already calibrated these: nothing to do but scale.
            None => {
                for (dst, p) in out.chunks_exact_mut(3).zip(pixels.data.iter()) {
                    dst[0] = clean(p[0]);
                    dst[1] = clean(p[1]);
                    dst[2] = clean(p[2]);
                }
            }
        },
        // Never calibrated, with or without `recon`: no colour matrix runs on
        // these, so there is nothing to clip and nothing outside the gamut.
        Intermediate::Monochrome(pixels) => {
            for (dst, p) in out.chunks_exact_mut(3).zip(pixels.data.iter()) {
                let v = clean(*p);
                dst[0] = v;
                dst[1] = v;
                dst[2] = v;
            }
        }
        // Unreachable in practice: `three_color_capable` sends 4-colour CFAs down
        // the `Calibrate` path, and Calibrate always folds FourColor to RGB. Kept
        // so an unexpected sensor drops its emerald channel rather than failing.
        Intermediate::FourColor(pixels) => {
            for (dst, p) in out.chunks_exact_mut(3).zip(pixels.data.iter()) {
                dst[0] = clean(p[0]);
                dst[1] = clean(p[1]);
                dst[2] = clean(p[2]);
            }
        }
    }
    Some((out, w, h))
}

/// True when the superpixel (fast, half-resolution) debayer can be used.
fn superpixel_capable(raw: &RawImage) -> bool {
    if raw.cpp != 1 || raw.fuji_rotation_width.is_some() {
        return false;
    }
    match &raw.photometric {
        RawPhotometricInterpretation::Cfa(cfg) => {
            cfg.sensor == SensorType::Bayer && cfg.cfa.is_rgb() && cfg.colors.plane_count() == 3
        }
        _ => false,
    }
}

/// Debayer at half resolution, then let rawler apply the default crop (and, when
/// `calibrated`, its own white balance and colour matrix) to the 3-channel image.
///
/// rawler 0.8 (crates.io) hard-codes PPG inside `develop_intermediate` and has no
/// `RawDevelop::demosaic_algorithm` knob (that field only exists on RapidRAW's
/// DngLab fork), and the calibration helpers are `pub(crate)`. Feeding the
/// superpixel output back in as a `cpp == 3` `RawImage` is the way to reach the
/// fast path through the public API.
fn develop_half(mut raw: RawImage, calibrated: bool) -> Option<Intermediate> {
    raw.apply_scaling().ok()?;

    let cfg = match &raw.photometric {
        RawPhotometricInterpretation::Cfa(cfg) => cfg.clone(),
        _ => return None,
    };

    let full = Rect::new(Point::zero(), Dim2::new(raw.width, raw.height));
    let roi = raw.active_area.unwrap_or(full);
    let data = raw.data.as_f32().into_owned();
    if data.len() != raw.width * raw.height {
        return None;
    }
    let pixels = PixF32::new_with(data, raw.width, raw.height);
    let rgb = Superpixel3Channel::new().demosaic(&pixels, &cfg.cfa, &cfg.colors, roi);
    drop(pixels);

    let dim = rgb.dim();
    if dim.w == 0 || dim.h == 0 {
        return None;
    }

    // Default crop is expressed in full-sensor coordinates: rebase it onto the
    // active area (rawler only does that itself when Demosaic ran) and halve it.
    let crop = raw.crop_area.or(raw.active_area).and_then(|mut c| {
        if let Some(active) = raw.active_area {
            if c.p.x < active.p.x || c.p.y < active.p.y || c.d.w > active.d.w || c.d.h > active.d.h {
                return None;
            }
            c = c.adapt(&active);
        }
        c.scale(0.5);
        if c.p.x + c.d.w > dim.w || c.p.y + c.d.h > dim.h || c.d.w == 0 || c.d.h == 0 {
            None
        } else {
            Some(c)
        }
    });

    raw.width = dim.w;
    raw.height = dim.h;
    raw.cpp = 3;
    raw.data = RawImageData::Float(rgb.into_flatten());
    raw.photometric = RawPhotometricInterpretation::LinearRaw;
    raw.active_area = None;
    raw.crop_area = crop;
    raw.blackareas = Vec::new();

    let steps: &[ProcessingStep] = if calibrated {
        &LINEAR_STEPS_PREDEBAYERED
    } else {
        &LINEAR_STEPS_PREDEBAYERED_UNCALIBRATED
    };
    RawDevelop::new_with(steps).develop_intermediate(&raw).ok()
}

/// [QA 1a0fb4fc] The camera crop of a Panasonic RW2 (IFD0 tags CropTop 0x2F, CropLeft 0x30, CropBottom 0x31,
/// CropRight 0x32, full-sensor coordinates), read from the header only. None for any other file.
fn panasonic_crop(path: &PathBuf) -> Option<Rect> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut b = vec![0u8; 65536];
    let n = f.read(&mut b).ok()?;
    b.truncate(n);
    // RW2 / RWL: "IIU\0" (little-endian TIFF with Panasonic's magic 0x55).
    if b.len() < 8 || &b[0..4] != b"IIU\0" {
        return None;
    }
    let u16_at = |o: usize| -> Option<u16> { b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]])) };
    let u32_at = |o: usize| -> Option<u32> { b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])) };
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    let (mut top, mut left, mut bottom, mut right) = (None, None, None, None);
    for i in 0..count.min(1024) {
        let e = ifd + 2 + 12 * i;
        let (tag, typ) = (u16_at(e)?, u16_at(e + 2)?);
        let v = match typ {
            3 => u16_at(e + 8)? as usize,
            4 => u32_at(e + 8)? as usize,
            _ => continue,
        };
        match tag {
            0x2f => top = Some(v),
            0x30 => left = Some(v),
            0x31 => bottom = Some(v),
            0x32 => right = Some(v),
            _ => {}
        }
    }
    let (t, l, bo, r) = (top?, left?, bottom?, right?);
    if r <= l || bo <= t {
        return None;
    }
    Some(Rect::new(Point::new(l, t), Dim2::new(r - l, bo - t)))
}

/// [QA 1a0fb4fc] Panasonic's embedded distortion correction (IFD0 tag 0x119 DistortionInfo of an RW2 / RWL:
/// 16 little-endian int16; word 7 = 1 when the camera asks for it, words 5 / 8 / 4 / 11 = scale and the
/// r³ / r⁵ / r⁷ coefficients, /32768 — trou/panasonic-rw2 notes, darktable's `CORRECTION_TYPE_PANASONIC`).
/// Ru = Rd · (1 + a·Rd² + b·Rd⁴ + c·Rd⁶), radii normalised to the half-diagonal of the camera's frame, a / b / c
/// already multiplied by scale. Compacts and bridge cameras (FZ / LX / TZ) rely on it: without it the corners are
/// black and the lens barrels (ImageIO and Lightroom always apply it). None = no tag or correction off.
fn panasonic_distortion(path: &PathBuf) -> Option<[f32; 3]> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut b = vec![0u8; 65536];
    let n = f.read(&mut b).ok()?;
    b.truncate(n);
    if b.len() < 8 || &b[0..4] != b"IIU\0" {
        return None;
    }
    let u16_at = |o: usize| -> Option<u16> { b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]])) };
    let u32_at = |o: usize| -> Option<u32> { b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])) };
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    for i in 0..count.min(1024) {
        let e = ifd + 2 + 12 * i;
        if u16_at(e)? != 0x119 || u32_at(e + 4)? != 32 {
            continue;
        }
        let off = u32_at(e + 8)? as usize;
        let w = |k: usize| -> Option<f32> { u16_at(off + 2 * k).map(|v| v as i16 as f32) };
        if (w(7)? as i32) & 0x0f != 1 {
            return None;
        }
        let scale = 1.0 / (1.0 + w(5)? / 32768.0);
        let (a, bb, c) = (scale * w(8)? / 32768.0, scale * w(4)? / 32768.0, scale * w(11)? / 32768.0);
        if !(scale.is_finite() && scale > 0.0) || (a == 0.0 && bb == 0.0 && c == 0.0) {
            return None;
        }
        return Some([a, bb, c]);
    }
    None
}

/// Resample `src` (w×h RGB f32) through the Panasonic polynomial: each output pixel at radius Ru reads the source at
/// Rd (bilinear), Rd from a 4096-knot table of the inverted polynomial. Same size out; a sample that falls outside
/// the frame clamps to the edge (the camera's coefficients keep the frame inside the image circle).
fn apply_panasonic_distortion(src: &[f32], w: usize, h: usize, k: [f32; 3]) -> Vec<f32> {
    let [a, b, c] = k;
    let poly = |rd: f32| -> f32 {
        let r2 = rd * rd;
        rd * (1.0 + r2 * (a + r2 * (b + r2 * c)))
    };
    // Ru → Rd/Ru, Ru in [0, 1.5]; Newton from Rd = Ru.
    const KNOTS: usize = 4096;
    const RMAX: f32 = 1.5;
    let mut lut = vec![1.0f32; KNOTS + 1];
    for (i, slot) in lut.iter_mut().enumerate().skip(1) {
        let ru = RMAX * i as f32 / KNOTS as f32;
        let mut rd = ru;
        for _ in 0..8 {
            let r2 = rd * rd;
            let d = 1.0 + r2 * (3.0 * a + r2 * (5.0 * b + r2 * 7.0 * c));
            if !(d.abs() > 1e-6) {
                break;
            }
            rd -= (poly(rd) - ru) / d;
        }
        *slot = if rd.is_finite() && rd > 0.0 { rd / ru } else { 1.0 };
    }
    lut[0] = lut[1];
    let (cx, cy) = ((w as f32 - 1.0) * 0.5, (h as f32 - 1.0) * 0.5);
    let half_diag = (cx * cx + cy * cy).sqrt().max(1.0);
    let mut out = vec![0.0f32; w * h * 3];
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16);
    let rows_per = (h + threads - 1) / threads;
    std::thread::scope(|s| {
        for (t, chunk) in out.chunks_mut(rows_per * w * 3).enumerate() {
            let lut = &lut;
            s.spawn(move || {
                let y0 = t * rows_per;
                for (ry, row) in chunk.chunks_mut(w * 3).enumerate() {
                    let dy = (y0 + ry) as f32 - cy;
                    for x in 0..w {
                        let dx = x as f32 - cx;
                        let ru = (dx * dx + dy * dy).sqrt() / half_diag;
                        let fi = (ru / RMAX * KNOTS as f32).min(KNOTS as f32);
                        let i0 = fi as usize;
                        let i1 = (i0 + 1).min(KNOTS);
                        let t = fi - i0 as f32;
                        let m = lut[i0] + (lut[i1] - lut[i0]) * t;
                        let sx = (cx + dx * m).clamp(0.0, w as f32 - 1.0);
                        let sy = (cy + dy * m).clamp(0.0, h as f32 - 1.0);
                        let (x0, y0s) = (sx as usize, sy as usize);
                        let (x1, y1) = ((x0 + 1).min(w - 1), (y0s + 1).min(h - 1));
                        let (fx, fy) = (sx - x0 as f32, sy - y0s as f32);
                        for ch in 0..3 {
                            let p = |xx: usize, yy: usize| src[(yy * w + xx) * 3 + ch];
                            let top = p(x0, y0s) + (p(x1, y0s) - p(x0, y0s)) * fx;
                            let bot = p(x0, y1) + (p(x1, y1) - p(x0, y1)) * fx;
                            row[x * 3 + ch] = top + (bot - top) * fy;
                        }
                    }
                }
            });
        }
    });
    out
}

/// [hotpixels] What `rawler_decode_path_ex` asks beyond `half_size` (see the header).
const RAWLER_DECODE_HOT_PIXELS: u32 = 1;

#[repr(C)]
pub struct RawlerDecodeOptions {
    pub half_size: i32,
    pub flags: u32,
    pub hot_pixel_k: f32,
}

#[repr(C)]
#[derive(Default)]
pub struct RawlerDecodeStats {
    pub hot_pixels_replaced: u64,
    pub hot_pixels_examined: u64,
    pub hot_pixels_ms: f64,
    pub hot_pixels_ran: i32,
}

/// [Prism 3FR] Hasselblad 3FR / FFF: the file's own WhiteLevel (0xC61D, in the raw SubIFD) instead of the camera
/// TOML's. rawler 0.8 lets `make_whitelevel` win over the tag the TFR decoder reads (`RawImage::new`), and its
/// `hasselblad/h3d.toml` says 62914 while the files ("Hasselblad H3D", 14-bit data) write 16383: every such 3FR came
/// out 62914 / 16383 = 3.84× (1.94 EV) dark, with highlight recon and the hot-pixel test working against a clip level
/// the data never reaches. ImageIO, Adobe and the camera's own preview all use the tag. Other formats: untouched.
pub(crate) fn apply_tfr_file_white_level(path: &PathBuf, source: &RawSource, raw: &mut RawImage) {
    use rawler::formats::tiff::GenericTiffReader;
    use rawler::formats::tiff::reader::TiffReader;
    use rawler::tags::TiffCommonTag;
    let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default();
    if ext != "3fr" && ext != "fff" {
        return;
    }
    let Ok(tiff) = GenericTiffReader::new(&mut source.reader(), 0, 0, None, &[]) else { return };
    let Some(level) = tiff
        .find_first_ifd_with_tag(TiffCommonTag::WhiteLevel)
        .and_then(|ifd| ifd.get_entry(TiffCommonTag::WhiteLevel))
        .map(|entry| entry.force_u32(0))
    else {
        return;
    };
    let black = raw.blacklevel.levels.first().map(Rational::as_f32).unwrap_or(0.0);
    if level == 0 || (level as f32) <= black || level > u16::MAX as u32 {
        return;
    }
    if raw.whitelevel.0.iter().all(|&w| w == level) {
        return;
    }
    let n = raw.whitelevel.0.len().max(1);
    raw.whitelevel.0 = vec![level; n];
}

fn is_tfr_path(path: &PathBuf) -> bool {
    matches!(path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(), Some("3fr") | Some("fff"))
}

/// [Prism 3FR] Hasselblad 3FR / FFF: the file's own ColorMatrix1 / ColorMatrix2 (DNG tags in IFD0, XYZ → camera, the
/// matrix Hasselblad wrote for THIS body) instead of the camera TOML's. rawler 0.8 reads only the TOML for 3FR
/// (`hasselblad/h3d.toml`: one D65 matrix that is not the file's: R/B ratio of a D65 white 0.387 vs 0.397, but G row
/// -0.60 / 1.45 / 0.16 vs -0.38 / 1.21 / 0.20), so warm saturated colours came out pink / magenta (a tungsten portrait:
/// skin and walls red-magenta where ImageIO, Adobe and the camera's own preview are amber) and the greens low. ImageIO
/// and the camera use the tag. A file without a usable 3×3 matrix keeps the TOML. Other formats: untouched.
pub(crate) fn apply_tfr_file_color_matrix(path: &PathBuf, source: &RawSource, raw: &mut RawImage) {
    use rawler::formats::tiff::GenericTiffReader;
    use rawler::formats::tiff::reader::TiffReader;
    use rawler::tags::DngTag;
    let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default();
    if ext != "3fr" && ext != "fff" {
        return;
    }
    let Ok(tiff) = GenericTiffReader::new(&mut source.reader(), 0, 0, None, &[]) else { return };
    let mut found: Vec<(Illuminant, Vec<f32>)> = Vec::new();
    for (cal, mat) in [(DngTag::CalibrationIlluminant1, DngTag::ColorMatrix1), (DngTag::CalibrationIlluminant2, DngTag::ColorMatrix2)] {
        let Some(ifd) = tiff.find_first_ifd_with_tag(mat) else { continue };
        let Some(entry) = ifd.get_entry(mat) else { continue };
        if entry.count() != 9 {
            continue;
        }
        let values: Vec<f32> = (0..9).map(|i| entry.force_f32(i)).collect();
        if !values.iter().all(|v| v.is_finite()) || values.iter().all(|v| *v == 0.0) {
            continue;
        }
        // DNG: an absent CalibrationIlluminant is 21 (D65), as rawler's own DNG reader takes it.
        let code = ifd.get_entry(cal).map(|e| e.force_u16(0)).unwrap_or(21);
        let illuminant: Illuminant = code.try_into().unwrap_or(Illuminant::D65);
        found.push((illuminant, values));
    }
    if found.is_empty() {
        return;
    }
    raw.color_matrix.clear();
    for (illuminant, values) in found {
        raw.color_matrix.insert(illuminant, values);
    }
}

fn decode_impl(path: PathBuf, half_size: bool) -> Option<RawlerImage> {
    decode_impl_ex(path, half_size, 0, hotpixels::DEFAULT_K, None)
}

fn decode_impl_ex(
    path: PathBuf,
    half_size: bool,
    flags: u32,
    hot_pixel_k: f32,
    stats: Option<&mut RawlerDecodeStats>,
) -> Option<RawlerImage> {
    let source = RawSource::new(&path).ok()?;
    let decoder = rawler::get_decoder(&source).ok()?;
    let params = RawDecodeParams::default();

    let mut raw = decoder.raw_image(&source, &params, false).ok()?;
    let metadata = decoder.raw_metadata(&source, &params).ok();
    apply_tfr_file_white_level(&path, &source, &mut raw);   // [Prism 3FR] before every use of the white level
    apply_tfr_file_color_matrix(&path, &source, &mut raw);  // [Prism 3FR] the file's own matrix, not the camera TOML's

    // [QA 1a0fb4fc] Panasonic RW2: rawler 0.8 takes the sensor borders (0x04–0x07) as BOTH the active area and the
    // default crop, and only reads the camera's crop (0x2F–0x32: the aspect the photo was shot in, e.g. the 4:3
    // 4864×3648 of a DMC-FZ2500 out of its 5472×3648 sensor) when the borders are missing. Apply it when it lies
    // inside the active area: the photo comes out as the camera framed it (and as ImageIO / the catalog size it).
    if let (Some(active), Some(crop)) = (raw.active_area, panasonic_crop(&path)) {
        let inside = crop.p.x >= active.p.x
            && crop.p.y >= active.p.y
            && crop.p.x + crop.d.w <= active.p.x + active.d.w
            && crop.p.y + crop.d.h <= active.p.y + active.d.h;
        if inside && crop.d.w > 0 && crop.d.h > 0 && (crop.d.w < active.d.w || crop.d.h < active.d.h) {
            raw.crop_area = Some(crop);
        }
    }

    // [hotpixels] On the mosaic, with the file's own levels, before anything scales or demosaics it.
    if flags & RAWLER_DECODE_HOT_PIXELS != 0 {
        let k = if hot_pixel_k.is_finite() && hot_pixel_k > 0.0 { hot_pixel_k } else { hotpixels::DEFAULT_K };
        if let Some(st) = hotpixels::suppress(&mut raw, k) {
            if let Some(out) = stats {
                out.hot_pixels_replaced = st.replaced;
                out.hot_pixels_examined = st.examined;
                out.hot_pixels_ms = st.millis;
                out.hot_pixels_ran = 1;
            }
        }
    }

    // --- white-level headroom -------------------------------------------------
    // rawler normalises to [0,1] against the white level and its colour-matrix
    // step desaturates anything above 1.0. Lift the white level to u32::MAX so
    // nothing is compressed, then rescale afterwards so the sensor white level
    // lands on 1.0 and real highlights keep their headroom (RapidRAW's trick).
    let original_white = raw.whitelevel.0.first().copied().unwrap_or(u16::MAX as u32) as f32;
    let original_black = raw.blacklevel.levels.first().map(Rational::as_f32).unwrap_or(0.0);
    for level in raw.whitelevel.0.iter_mut() {
        *level = u32::MAX;
    }
    let rescale = (u32::MAX as f32 - original_black) / (original_white - original_black).max(1.0);

    let wb = raw.wb_coeffs;

    // Built from the *pre-develop* image: the WB coefficients and the colour
    // matrix ARE the colour stage, and this crate runs it itself (see
    // `HighlightRecon`) whenever it can — which needs a usable 3×3 matrix, a sane
    // white balance and a sensor that develops to three channels. When any of
    // those is missing, `Calibrate` stays with rawler and the pipeline is exactly
    // the historical one. Cheap: a couple of 3×3 products, and `develop_half`'s
    // clone keeps the fields it reads untouched.
    // [Prism 3FR] saturation of the 16-bit data in units of the file's WhiteLevel (see `HighlightRecon::clip`).
    let clip = if is_tfr_path(&path) { (u16::MAX as f32 - original_black) / (original_white - original_black).max(1.0) } else { 1.0 };
    let recon = highlight_recon(&raw, clip).filter(|_| three_color_capable(&raw));
    let calibrated = recon.is_none();

    let mut did_halve = false;
    let mut developed = None;
    if half_size && superpixel_capable(&raw) {
        developed = develop_half(raw.clone(), calibrated);
        did_halve = developed.is_some();
    }
    if developed.is_none() {
        // Full-resolution PPG develop (also the fallback when the superpixel
        // path refused this sensor).
        let steps: &[ProcessingStep] = if calibrated { &LINEAR_STEPS } else { &LINEAR_STEPS_UNCALIBRATED };
        developed = RawDevelop::new_with(steps).develop_intermediate(&raw).ok();
    }
    let developed = developed?;
    drop(raw);

    let (mut pixels, mut w, mut h) = intermediate_to_rgb(developed, rescale, recon.as_ref())?;

    // half_size was asked for but the sensor could not take the fast path.
    if half_size && !did_halve {
        let (p, nw, nh) = box_halve(&pixels, w, h);
        pixels = p;
        w = nw;
        h = nh;
    }

    // [QA 1a0fb4fc] The camera's embedded distortion correction (Panasonic RW2), on the camera's frame, before the
    // rotation: radial, about the centre of the cropped picture.
    if let Some(k) = panasonic_distortion(&path) {
        if std::env::var_os("RAWLER_FFI_NO_DISTORTION").is_none() {
            pixels = apply_panasonic_distortion(&pixels, w, h, k);
        }
    }

    // --- metadata + orientation ----------------------------------------------
    let mut meta = RawlerMeta::default();
    let mut orientation_tag = 0u16;

    if let Some(md) = &metadata {
        fill_cstr_field(&mut meta.camera_make, &md.make);
        fill_cstr_field(&mut meta.camera_model, &md.model);
        let exif = &md.exif;
        orientation_tag = exif.orientation.unwrap_or(0);
        meta.iso = exif
            .iso_speed_ratings
            .map(|v| v as f32)
            .or(exif.iso_speed.map(|v| v as f32))
            .or(exif.recommended_exposure_index.map(|v| v as f32))
            .unwrap_or(0.0);
        meta.exposure_time = rational_f32(exif.exposure_time);
        meta.aperture = match rational_f32(exif.fnumber) {
            0.0 => {
                // APEX aperture value: f = 2^(Av/2).
                let av = rational_f32(exif.aperture_value);
                if av > 0.0 { (av * 0.5).exp2() } else { 0.0 }
            }
            f => f,
        };
        meta.focal_length = rational_f32(exif.focal_length);
    }

    let orientation = if orientation_tag >= 1 && orientation_tag <= 8 {
        Orientation::from_u16(orientation_tag)
    } else {
        Orientation::Normal
    };
    meta.orientation = if orientation_tag == 0 { 1 } else { orientation_tag as i32 };

    if wb.iter().take(3).all(|c| c.is_finite() && *c > 0.0) {
        meta.wb_r = wb[0];
        meta.wb_g = wb[1];
        meta.wb_b = wb[2];
    }

    let (pixels, w, h) = apply_orientation(pixels, w, h, orientation);

    let mut boxed = pixels.into_boxed_slice();
    let ptr = boxed.as_mut_ptr();
    std::mem::forget(boxed);

    Some(RawlerImage {
        width: w as i32,
        height: h as i32,
        channels: 3,
        pixels: ptr,
        meta,
    })
}

// ---------------------------------------------------------------------------
// Embedded preview JPEG
// ---------------------------------------------------------------------------

/// Length of the pictorial (baseline/progressive DCT) JPEG starting at `off`,
/// or `None` when the bytes are not one.
///
/// The `None` case matters: CR2 stores its *raw* sensor data as a lossless JPEG
/// (SOF3) in a compression-6 strip, which is the biggest "JPEG" in the file and
/// must not be handed back as a preview.
fn pictorial_jpeg_len(buf: &[u8], off: usize, limit: usize) -> Option<usize> {
    if buf.len() < off + 4 || buf[off] != 0xFF || buf[off + 1] != 0xD8 || buf[off + 2] != 0xFF {
        return None;
    }
    let end_cap = buf.len().min(off.saturating_add(limit));
    let mut i = off + 2;
    let mut pictorial = false;

    while i + 1 < end_cap {
        if buf[i] != 0xFF {
            return None;
        }
        // Fill bytes.
        let mut m = i + 1;
        while m < end_cap && buf[m] == 0xFF {
            m += 1;
        }
        if m >= end_cap {
            return None;
        }
        let marker = buf[m];
        i = m + 1;
        match marker {
            // Standalone markers, no payload.
            0x01 | 0xD0..=0xD7 => continue,
            0xD9 => return if pictorial { Some(i - off) } else { None },
            0xC3 | 0xC7 | 0xCB | 0xCF => return None, // lossless / differential-lossless SOF
            _ => {}
        }
        if i + 1 >= end_cap {
            return None;
        }
        let seg = ((buf[i] as usize) << 8) | buf[i + 1] as usize;
        if seg < 2 {
            return None;
        }
        if matches!(marker, 0xC0 | 0xC1 | 0xC2 | 0xC5 | 0xC6 | 0xC9 | 0xCA | 0xCD | 0xCE) {
            pictorial = true;
        }
        i += seg;
        if marker == 0xDA {
            // Entropy-coded data: walk to EOI, skipping stuffed bytes and RSTn.
            while i + 1 < end_cap {
                if buf[i] == 0xFF {
                    let n = buf[i + 1];
                    if n == 0xD9 {
                        return if pictorial { Some(i + 2 - off) } else { None };
                    }
                    if n == 0x00 || (0xD0..=0xD7).contains(&n) || n == 0xFF {
                        i += 2;
                        continue;
                    }
                    // Another marker segment (progressive scans).
                    break;
                }
                i += 1;
            }
            if i + 1 >= end_cap {
                return None;
            }
        }
    }
    None
}

/// Collect (offset, length) candidates for embedded JPEGs by walking the TIFF
/// IFD chain, including SubIFDs. Ported from RapidRAW's
/// `image_loader::largest_tiff_jpeg_preview`, minus the decode.
fn tiff_jpeg_candidates(buf: &[u8]) -> Vec<(usize, usize)> {
    let le = match buf.get(..4) {
        Some([0x49, 0x49, 0x2A, 0x00]) => true,
        Some([0x4D, 0x4D, 0x00, 0x2A]) => false,
        _ => return Vec::new(),
    };
    let rd16 = |o: usize| -> Option<u64> {
        let b: [u8; 2] = buf.get(o..o + 2)?.try_into().ok()?;
        Some(if le { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) } as u64)
    };
    let rd32 = |o: usize| -> Option<u64> {
        let b: [u8; 4] = buf.get(o..o + 4)?.try_into().ok()?;
        Some(if le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) } as u64)
    };

    let mut candidates: Vec<(usize, usize)> = Vec::new();
    let mut queue: Vec<u64> = match rd32(4) {
        Some(v) => vec![v],
        None => return candidates,
    };
    let mut seen: HashSet<u64> = HashSet::new();

    while let Some(ifd) = queue.pop() {
        if !seen.insert(ifd) || seen.len() > 64 {
            continue;
        }
        let Some(n) = rd16(ifd as usize) else { continue };

        let mut compression: u64 = 0;
        let mut strip: Option<(u64, u64)> = None;
        let mut old_jpeg: Option<(u64, u64)> = None;

        for i in 0..n {
            let e = ifd as usize + 2 + (i as usize) * 12;
            let (Some(tag), Some(count), Some(val)) = (rd16(e), rd32(e + 4), rd32(e + 8)) else {
                continue;
            };
            match tag {
                259 => compression = val,
                // StripOffsets / StripByteCounts (single strip only).
                273 if count == 1 => strip = Some((val, strip.map_or(0, |s| s.1))),
                279 if count == 1 => strip = strip.map(|s| (s.0, val)).or(Some((0, val))),
                // JPEGInterchangeFormat / -Length.
                513 => old_jpeg = Some((val, old_jpeg.map_or(0, |s| s.1))),
                514 => old_jpeg = old_jpeg.map(|s| (s.0, val)).or(Some((0, val))),
                // SubIFDs.
                330 => {
                    if count == 1 {
                        queue.push(val);
                    } else {
                        for j in 0..count.min(8) {
                            if let Some(p) = rd32(val as usize + (j as usize) * 4) {
                                queue.push(p);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        if matches!(compression, 6 | 7)
            && let Some(s) = strip
        {
            candidates.push((s.0 as usize, s.1 as usize));
        }
        if let Some(oj) = old_jpeg {
            candidates.push((oj.0 as usize, oj.1 as usize));
        }
        if let Some(next) = rd32(ifd as usize + 2 + (n as usize) * 12)
            && next != 0
        {
            queue.push(next);
        }
    }

    candidates.sort_by_key(|&(_, len)| std::cmp::Reverse(len));
    candidates
}

/// Brute-force fallback for non-TIFF containers (CR3's ISO-BMFF boxes, RAF's
/// JPEG blob): find every SOI and keep the largest pictorial JPEG.
fn scan_jpeg(buf: &[u8]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    let mut i = 0usize;
    let mut tried = 0usize;
    while i + 3 < buf.len() && tried < 64 {
        if buf[i] == 0xFF && buf[i + 1] == 0xD8 && buf[i + 2] == 0xFF {
            tried += 1;
            if let Some(len) = pictorial_jpeg_len(buf, i, buf.len() - i) {
                if best.map_or(true, |(_, bl)| len > bl) {
                    best = Some((i, len));
                }
                i += len;
                continue;
            }
        }
        i += 1;
    }
    best
}

fn thumbnail_impl(path: PathBuf) -> Option<Vec<u8>> {
    let source = RawSource::new(&path).ok()?;
    let buf = source.buf();

    for (off, len) in tiff_jpeg_candidates(buf) {
        if len == 0 || off >= buf.len() || off + len > buf.len() {
            continue;
        }
        if pictorial_jpeg_len(buf, off, len).is_some() {
            return Some(buf[off..off + len].to_vec());
        }
    }

    let (off, len) = scan_jpeg(buf)?;
    Some(buf[off..off + len].to_vec())
}

// ---------------------------------------------------------------------------
// Length-prefixed byte buffers (rawler_free_bytes gets no length back).
// ---------------------------------------------------------------------------

const BYTES_HDR: usize = 8;
const BYTES_ALIGN: usize = 8;

fn bytes_into_raw(data: &[u8]) -> Option<*mut u8> {
    let layout = Layout::from_size_align(BYTES_HDR + data.len(), BYTES_ALIGN).ok()?;
    unsafe {
        let base = alloc(layout);
        if base.is_null() {
            return None;
        }
        ptr::write_unaligned(base as *mut u64, data.len() as u64);
        ptr::copy_nonoverlapping(data.as_ptr(), base.add(BYTES_HDR), data.len());
        Some(base.add(BYTES_HDR))
    }
}

// ---------------------------------------------------------------------------
// C ABI
// ---------------------------------------------------------------------------

/// Decode + develop the RAW at `path`. See `include/rawler_ffi.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rawler_decode_path(path: *const c_char, half_size: i32) -> *mut RawlerImage {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let path = path_from_c(path)?;
        decode_impl(path, half_size != 0)
    }));
    match result {
        Ok(Some(img)) => Box::into_raw(Box::new(img)),
        _ => ptr::null_mut(),
    }
}

/// [hotpixels] `rawler_decode_path` with options (hot-pixel suppression on the mosaic) and stats. See the header.
#[unsafe(no_mangle)]
pub extern "C" fn rawler_decode_path_ex(
    path: *const c_char,
    options: *const RawlerDecodeOptions,
    stats: *mut RawlerDecodeStats,
) -> *mut RawlerImage {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let path = path_from_c(path)?;
        let (half, flags, k) = if options.is_null() {
            (false, 0u32, hotpixels::DEFAULT_K)
        } else {
            let o = unsafe { &*options };
            (o.half_size != 0, o.flags, o.hot_pixel_k)
        };
        let mut local = RawlerDecodeStats::default();
        let img = decode_impl_ex(path, half, flags, k, Some(&mut local));
        if !stats.is_null() {
            unsafe { ptr::write(stats, local) };
        }
        img
    }));
    match result {
        Ok(Some(img)) => Box::into_raw(Box::new(img)),
        _ => ptr::null_mut(),
    }
}

/// Largest embedded preview JPEG, verbatim. See `include/rawler_ffi.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rawler_thumbnail_jpeg(path: *const c_char, out_len: *mut usize) -> *mut u8 {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let path = path_from_c(path)?;
        thumbnail_impl(path)
    }));
    let Ok(Some(bytes)) = result else {
        return ptr::null_mut();
    };
    if bytes.is_empty() {
        return ptr::null_mut();
    }
    match bytes_into_raw(&bytes) {
        Some(p) => {
            if !out_len.is_null() {
                unsafe { *out_len = bytes.len() };
            }
            p
        }
        None => ptr::null_mut(),
    }
}

/// Header sniff: 1 when rawler has a decoder for this file.
#[unsafe(no_mangle)]
pub extern "C" fn rawler_is_supported(path: *const c_char) -> i32 {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let Some(path) = path_from_c(path) else { return false };
        let Ok(source) = RawSource::new(&path) else { return false };
        rawler::get_decoder(&source).is_ok()
    }));
    match result {
        Ok(true) => 1,
        _ => 0,
    }
}

/// Free an image returned by `rawler_decode_path`. NULL is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn rawler_free_image(img: *mut RawlerImage) {
    if img.is_null() {
        return;
    }
    let _ = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
        let boxed = Box::from_raw(img);
        if !boxed.pixels.is_null() {
            let len = (boxed.width as usize) * (boxed.height as usize) * (boxed.channels as usize);
            drop(Box::from_raw(ptr::slice_from_raw_parts_mut(boxed.pixels, len)));
        }
    }));
}

/// Free a buffer returned by `rawler_thumbnail_jpeg`. NULL is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn rawler_free_bytes(p: *mut u8) {
    if p.is_null() {
        return;
    }
    let _ = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
        let base = p.sub(BYTES_HDR);
        let len = ptr::read_unaligned(base as *const u64) as usize;
        if let Ok(layout) = Layout::from_size_align(BYTES_HDR + len, BYTES_ALIGN) {
            dealloc(base, layout);
        }
    }));
}
