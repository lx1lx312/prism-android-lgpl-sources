//! enhance.rs — [PR-MAC-172] the sensor mosaic of a RAW and a DNG writer for it.
//!
//! Prism's Photo ▸ Enhance… (Denoise) runs a raw-domain model on the Bayer mosaic and writes the result as a new
//! DNG next to the original ("<name>-Enhanced-NR.dng", Lightroom's naming). This file gives the host:
//!
//!   * `rawler_mosaic_open`   — decode the sensor data WITHOUT developing it: the mosaic as f32, normalised per
//!                              CFA position ((v − black) / (white − black), NOT clipped), plus what a denoiser
//!                              needs (2×2 CFA colours, levels, active area, ISO, make / model);
//!   * `rawler_mosaic_write_dng` — the same RawImage with the host's (processed) mosaic, written as a DNG 1.4
//!                              with rawler's writer, exactly like DngLab's conversion: same black / white levels,
//!                              CFA, matrices, AsShotNeutral, EXIF, makernotes rule, embedded XMP (+
//!                              `prism:EnhancedFrom`), preview and thumbnail from the original file;
//!   * `rawler_mosaic_free`.
//!
//! Only 2×2 Bayer sensors with integer data are "supported" (the model works on packed RGGB): X-Trans (6×6),
//! LinearRaw / float DNGs and the rotated Fujifilm SuperCCD sensors are reported as such and not opened.
//! Every entry point is panic-safe like the rest of the crate.

use std::fs::File;
use std::io::BufWriter;
use std::os::raw::c_char;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr;

use rawler::decoders::{RawDecodeParams, WellKnownIFD};
use rawler::dng::writer::DngWriter;
use rawler::dng::{CropMode, DNG_VERSION_V1_4, DngCompression, DngPhotometricConversion};
use rawler::formats::tiff::{Entry, Value};
use rawler::rawimage::{RawImage, RawImageData, RawPhotometricInterpretation};
use rawler::rawsource::RawSource;
use rawler::tags::{DngTag, ExifTag, TiffCommonTag};

use crate::{fill_cstr_field, path_from_c};

/// Status codes of `rawler_mosaic_open` (`out_status`).
pub const RAWLER_MOSAIC_OK: i32 = 0;
pub const RAWLER_MOSAIC_DECODE_FAILED: i32 = 1;
pub const RAWLER_MOSAIC_NOT_BAYER: i32 = 2;
pub const RAWLER_MOSAIC_UNSUPPORTED_DATA: i32 = 3;

#[repr(C)]
pub struct RawlerMosaicInfo {
    pub width: i32,
    pub height: i32,
    /// Colour at (0,0) (0,1) (1,0) (1,1) of the FULL frame: 0 R, 1 G, 2 B.
    pub cfa: [i32; 4],
    /// Black / white level at the same four positions (sensor units).
    pub black: [f32; 4],
    pub white: [f32; 4],
    pub active_x: i32,
    pub active_y: i32,
    pub active_w: i32,
    pub active_h: i32,
    pub iso: f32,
    /// As-shot white balance multipliers R, G, B, (E) — 1 when the file has none.
    pub wb: [f32; 4],
    /// EXIF orientation 1…8 of the photo (the mosaic is NOT rotated).
    pub orientation: i32,
    pub camera_make: [c_char; 64],
    pub camera_model: [c_char; 64],
}

pub struct RawlerMosaic {
    path: PathBuf,
    raw: RawImage,
    normalized: Vec<f32>,
    black: [f32; 4],
    white: [f32; 4],
}

fn open_impl(path: PathBuf, info: &mut RawlerMosaicInfo) -> Result<RawlerMosaic, i32> {
    let source = RawSource::new(&path).map_err(|_| RAWLER_MOSAIC_DECODE_FAILED)?;
    let decoder = rawler::get_decoder(&source).map_err(|_| RAWLER_MOSAIC_DECODE_FAILED)?;
    let params = RawDecodeParams::default();
    let mut raw = decoder.raw_image(&source, &params, false).map_err(|_| RAWLER_MOSAIC_DECODE_FAILED)?;
    crate::apply_tfr_file_white_level(&path, &source, &mut raw);   // [Prism 3FR] the DNG gets the file's white level
    crate::apply_tfr_file_color_matrix(&path, &source, &mut raw);  // [Prism 3FR] ... and the file's colour matrix
    if let Ok(md) = decoder.raw_metadata(&source, &params) {
        fill_cstr_field(&mut info.camera_make, &md.make);
        fill_cstr_field(&mut info.camera_model, &md.model);
        info.iso = md
            .exif
            .iso_speed_ratings
            .map(|v| v as f32)
            .or(md.exif.iso_speed.map(|v| v as f32))
            .or(md.exif.recommended_exposure_index.map(|v| v as f32))
            .unwrap_or(0.0);
    }
    info.wb = if raw.wb_coeffs[0].is_nan() { [1.0; 4] } else { raw.wb_coeffs };
    info.orientation = raw.orientation.to_u16() as i32;
    info.width = raw.width as i32;
    info.height = raw.height as i32;

    let cfa = match &raw.photometric {
        RawPhotometricInterpretation::Cfa(config) => config.cfa.clone(),
        _ => return Err(RAWLER_MOSAIC_NOT_BAYER),
    };
    if raw.cpp != 1 || cfa.width != 2 || cfa.height != 2 || cfa.unique_colors() != 3 {
        return Err(RAWLER_MOSAIC_NOT_BAYER);
    }
    if raw.camera.find_hint("fuji_rotation") || raw.camera.find_hint("fuji_rotation_alt") {
        return Err(RAWLER_MOSAIC_NOT_BAYER);
    }
    let data = match &raw.data {
        RawImageData::Integer(v) => v,
        RawImageData::Float(_) => return Err(RAWLER_MOSAIC_UNSUPPORTED_DATA),
    };
    for (i, (r, c)) in [(0, 0), (0, 1), (1, 0), (1, 1)].iter().enumerate() {
        let colour = cfa.color_at(*r, *c);
        if colour > 2 {
            return Err(RAWLER_MOSAIC_NOT_BAYER);
        }
        info.cfa[i] = colour as i32;
    }
    let black = raw.blacklevel.as_bayer_array();
    let white = raw.whitelevel.as_bayer_array();
    if (0..4).any(|i| white[i] <= black[i]) {
        return Err(RAWLER_MOSAIC_UNSUPPORTED_DATA);
    }
    info.black = black;
    info.white = white;
    let full = (0, 0, raw.width, raw.height);
    let (ax, ay, aw, ah) = raw.active_area.map(|a| (a.p.x, a.p.y, a.d.w, a.d.h)).unwrap_or(full);
    info.active_x = ax as i32;
    info.active_y = ay as i32;
    info.active_w = aw as i32;
    info.active_h = ah as i32;

    let w = raw.width;
    let scale: [f32; 4] = std::array::from_fn(|i| 1.0 / (white[i] - black[i]));
    let mut normalized = vec![0f32; data.len()];
    for (y, (dst, src)) in normalized.chunks_exact_mut(w).zip(data.chunks_exact(w)).enumerate() {
        let row = (y & 1) * 2;
        for (x, (d, s)) in dst.iter_mut().zip(src.iter()).enumerate() {
            let p = row + (x & 1);
            *d = (*s as f32 - black[p]) * scale[p];
        }
    }
    Ok(RawlerMosaic { path, raw, normalized, black, white })
}

/// `prism:EnhancedFrom` added to the original's XMP packet (or a packet of its own).
fn xpacket_with_origin(original: Option<Vec<u8>>, enhanced_from: &str) -> Option<Vec<u8>> {
    if enhanced_from.is_empty() {
        return original;
    }
    let attr = format!(
        " xmlns:prism=\"http://ns.prism.app/1.0/\" prism:EnhancedFrom=\"{}\"",
        enhanced_from.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;")
    );
    if let Some(bytes) = &original {
        if let Ok(text) = std::str::from_utf8(bytes) {
            if !text.contains("xmlns:prism=") {
                if let Some(start) = text.find("<rdf:Description") {
                    let at = start + "<rdf:Description".len();
                    let mut out = String::with_capacity(text.len() + attr.len());
                    out.push_str(&text[..at]);
                    out.push_str(&attr);
                    out.push_str(&text[at..]);
                    return Some(out.into_bytes());
                }
            }
        }
        return original;
    }
    Some(
        format!(
            "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  <rdf:Description rdf:about=\"\"{}/>\n </rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>",
            attr
        )
        .into_bytes(),
    )
}

fn write_impl(m: &RawlerMosaic, pixels: &[f32], out: PathBuf, software: &str, enhanced_from: &str) -> Option<()> {
    if pixels.len() != m.normalized.len() {
        return None;
    }
    let mut raw = m.raw.clone();
    let w = raw.width;
    let mut data = vec![0u16; pixels.len()];
    for (y, (dst, src)) in data.chunks_exact_mut(w).zip(pixels.chunks_exact(w)).enumerate() {
        let row = (y & 1) * 2;
        for (x, (d, s)) in dst.iter_mut().zip(src.iter()).enumerate() {
            let p = row + (x & 1);
            let v = s * (m.white[p] - m.black[p]) + m.black[p];
            *d = if v.is_finite() { v.round().clamp(0.0, 65535.0) as u16 } else { 0 };
        }
    }
    raw.data = RawImageData::Integer(data);

    let source = RawSource::new(&m.path).ok()?;
    let decoder = rawler::get_decoder(&source).ok()?;
    let params = RawDecodeParams::default();
    let metadata = decoder.raw_metadata(&source, &params).ok()?;

    let file = File::create(&out).ok()?;
    let mut stream = BufWriter::new(file);
    let mut dng = DngWriter::new(&mut stream, DNG_VERSION_V1_4).ok()?;
    {
        let mut sub = dng.subframe(0);
        sub.raw_image(&raw, CropMode::Best, DngCompression::Lossless, DngPhotometricConversion::Original, 1)
            .ok()?;
        if let Ok(Some(ifd)) = decoder.ifd(WellKnownIFD::VirtualDngRawTags) {
            sub.ifd_mut().copy(ifd.value_iter());
        }
        sub.finalize().ok()?;
    }
    // The original's embedded preview (a few hundred KB; Lightroom regenerates its own).
    if let Ok(Some(image)) = decoder.preview_image(&source, &params) {
        let mut preview = dng.subframe(1);
        if preview.preview(&image, 0.75).is_ok() {
            let _ = preview.finalize();
        }
        let _ = dng.thumbnail(&image);
    }
    dng.load_base_tags(&raw).ok()?;
    dng.load_metadata(&metadata).ok()?;
    if !dng.root_ifd().contains(ExifTag::Orientation) {
        dng.root_ifd_mut().add_tag(ExifTag::Orientation, raw.orientation.to_u16());
    }
    if let Ok(Some(root)) = decoder.ifd(WellKnownIFD::VirtualDngRootTags) {
        dng.root_ifd_mut().copy(root.value_iter());
        if let Some(Entry { value: Value::Short(v), .. }) = root.get_entry(DngTag::MakerNoteSafety).cloned() {
            if v.first().copied().unwrap_or(0) == 0 {
                dng.exif_ifd_mut().remove_tag(ExifTag::MakerNotes);
            }
        }
    }
    let xpacket = decoder.xpacket(&source, &params).ok().flatten();
    if let Some(packet) = xpacket_with_origin(xpacket, enhanced_from) {
        dng.xpacket(&packet).ok()?;
    }
    if !software.is_empty() {
        dng.root_ifd_mut().add_tag(TiffCommonTag::Software, software);
    }
    dng.close().ok()?;
    drop(stream);
    Some(())
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// See `include/rawler_ffi_mosaic.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rawler_mosaic_open(path: *const c_char, out_info: *mut RawlerMosaicInfo, out_status: *mut i32) -> *mut RawlerMosaic {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut info: RawlerMosaicInfo = unsafe { std::mem::zeroed() };
        let opened = match path_from_c(path) {
            Some(p) => open_impl(p, &mut info),
            None => Err(RAWLER_MOSAIC_DECODE_FAILED),
        };
        (opened, info)
    }));
    let (opened, info) = match result {
        Ok(r) => r,
        Err(_) => (Err(RAWLER_MOSAIC_DECODE_FAILED), unsafe { std::mem::zeroed() }),
    };
    if !out_info.is_null() {
        unsafe { ptr::write(out_info, info) };
    }
    let status = match &opened {
        Ok(_) => RAWLER_MOSAIC_OK,
        Err(code) => *code,
    };
    if !out_status.is_null() {
        unsafe { *out_status = status };
    }
    match opened {
        Ok(m) => Box::into_raw(Box::new(m)),
        Err(_) => ptr::null_mut(),
    }
}

/// The normalised mosaic, width × height f32, owned by the handle.
#[unsafe(no_mangle)]
pub extern "C" fn rawler_mosaic_pixels(m: *const RawlerMosaic) -> *const f32 {
    if m.is_null() {
        return ptr::null();
    }
    unsafe { (*m).normalized.as_ptr() }
}

/// Writes `pixels` (same layout and normalisation as `rawler_mosaic_pixels`) as a DNG at `out_path`.
/// 0 = written; 1 = failed (a partial file may exist: the caller writes to a temporary name and renames).
#[unsafe(no_mangle)]
pub extern "C" fn rawler_mosaic_write_dng(
    m: *const RawlerMosaic,
    pixels: *const f32,
    out_path: *const c_char,
    software: *const c_char,
    enhanced_from: *const c_char,
) -> i32 {
    if m.is_null() || pixels.is_null() {
        return 1;
    }
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let mosaic = unsafe { &*m };
        let pixels = unsafe { std::slice::from_raw_parts(pixels, mosaic.normalized.len()) };
        let out = path_from_c(out_path)?;
        write_impl(mosaic, pixels, out, &cstr(software), &cstr(enhanced_from))
    }));
    match result {
        Ok(Some(())) => 0,
        _ => 1,
    }
}

/// Frees a handle of `rawler_mosaic_open`. NULL is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn rawler_mosaic_free(m: *mut RawlerMosaic) {
    if m.is_null() {
        return;
    }
    let _ = panic::catch_unwind(AssertUnwindSafe(|| unsafe { drop(Box::from_raw(m)) }));
}
