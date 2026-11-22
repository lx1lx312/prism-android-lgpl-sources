//! proxy.rs — [Prism F2] the 2048 proxy DNG: a LinearRaw (3 samples per pixel, 16 bit) DNG written with rawler's writer,
//! and the camera's colour model as rawler knows it.
//!
//! Leo (29/10/2026): «dobbiamo creare una cache dng e lavorare su quella finché non viene fatto uno zoom 1:1», «anche gli
//! altri engine devono accedere alla cache raw». The host (SkiaRaw/IBRawProxyDNG.cpp) reduces the sensor data to camera RGB
//! at a 2048 long edge (black → 0, white → 65535, before white balance and matrix, default crop applied, NOT oriented) and
//! hands it here with the tags it collected. This file:
//!
//!   * `rawler_camera_color` — the file's XYZ → camera matrices (rawler's camera database for a non-DNG raw, the tags of a
//!     DNG, the file's own matrix for a Hasselblad 3FR), their illuminants and rawler's as-shot multipliers. Cheap: the
//!     decoder runs in "dummy" mode (no sensor data decoded).
//!   * `rawler_linear_write_dng` — the DNG 1.4: LinearRaw, LJ92 lossless tiles 256 × 256 (Compression 7, predictor 1),
//!     raw image in IFD 0; colour model (copied VERBATIM from the source when it is a DNG — matrices, calibration,
//!     analog balance, as-shot white, baseline exposure, the profile's HueSatMap / LookTable / ToneCurve / name / embed
//!     policy — else the host's matrices with 1 / 1 000 000 denominators), EXIF from rawler's metadata of the source (the
//!     host's fallback fields when rawler cannot read it), the source's Orientation, the original's final size, an XMP
//!     packet (prism:ProxyOf …). Never copied: opcode lists, noise profile, digests, maker notes, the source's XMP (an
//!     Adobe reader would apply its crs: settings to the proxy).
//!
//! Every entry point is panic-safe like the rest of the crate.

use std::fs::File;
use std::io::BufWriter;
use std::os::raw::c_char;
use std::panic::{self, AssertUnwindSafe};

use rawler::decoders::{RawDecodeParams, WellKnownIFD};
use rawler::dng::writer::DngWriter;
use rawler::dng::{DNG_VERSION_V1_4, DngCompression};
use rawler::formats::tiff::{Rational, SRational};
use rawler::imgop::xyz::Illuminant;
use rawler::rawsource::RawSource;
use rawler::tags::{DngTag, ExifTag, TiffCommonTag};

use crate::{fill_cstr_field, path_from_c};

/// See `include/rawler_ffi_proxy.h`.
#[repr(C)]
pub struct RawlerCameraColor {
    pub matrix_count: i32,
    pub illuminant: [u16; 2],
    pub matrix: [[f64; 9]; 2],
    pub wb: [f64; 3],
    pub is_dng: i32,
    pub make: [c_char; 64],
    pub model: [c_char; 64],
}

/// See `include/rawler_ffi_proxy.h`.
#[repr(C)]
pub struct RawlerLinearDngSpec {
    pub out_path: *const c_char,
    pub source_path: *const c_char,
    pub rgb: *const u16,
    pub width: u32,
    pub height: u32,
    pub orientation: u16,
    pub black_level: u16,
    pub copy_source_dng_color: i32,
    pub matrix_count: i32,
    pub illuminant: [u16; 2],
    pub matrix: [[f64; 9]; 2],
    pub baseline_exposure: f64,
    pub has_neutral: i32,
    pub as_shot_neutral: [f64; 3],
    pub make: *const c_char,
    pub model: *const c_char,
    pub unique_camera_model: *const c_char,
    pub original_width: u32,
    pub original_height: u32,
    pub iso: f64,
    pub exposure_time: f64,
    pub f_number: f64,
    pub focal_length: f64,
    pub lens_model: *const c_char,
    pub software: *const c_char,
    pub xmp_packet: *const c_char,
}

const DENOM: i32 = 1_000_000;

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn srational(v: f64) -> SRational {
    let n = (v * DENOM as f64).round();
    SRational::new(n.clamp(i32::MIN as f64, i32::MAX as f64) as i32, DENOM)
}

fn rational(v: f64) -> Rational {
    let v = if v.is_finite() && v > 0.0 { v } else { 0.0 };
    // 1 / 1 000 000 resolution while the value fits a u32 numerator, else the largest denominator that does.
    let mut d: u32 = DENOM as u32;
    while d > 1 && v * d as f64 > u32::MAX as f64 {
        d /= 10;
    }
    Rational::new((v * d as f64).round() as u32, d)
}

fn camera_color_impl(path: std::path::PathBuf, out: &mut RawlerCameraColor) -> Option<()> {
    let source = RawSource::new(&path).ok()?;
    let decoder = rawler::get_decoder(&source).ok()?;
    let params = RawDecodeParams::default();
    let mut raw = decoder.raw_image(&source, &params, true).ok()?;
    crate::apply_tfr_file_color_matrix(&path, &source, &mut raw); // [Prism 3FR] the file's own matrix, as the decode uses
    fill_cstr_field(&mut out.make, &raw.clean_make);
    fill_cstr_field(&mut out.model, &raw.clean_model);
    out.is_dng = if decoder.ifd(WellKnownIFD::VirtualDngRootTags).ok().flatten().is_some() { 1 } else { 0 };
    out.wb = if raw.wb_coeffs[0].is_nan() || !raw.wb_coeffs[..3].iter().all(|v| v.is_finite() && *v > 0.0) {
        [1.0; 3]
    } else {
        [raw.wb_coeffs[0] as f64, raw.wb_coeffs[1] as f64, raw.wb_coeffs[2] as f64]
    };
    // The writer's own order: A first (slot 1), then D65 / D50 (slot 2); one matrix = slot 1. 3×3 only (a 4-colour CFA's
    // 4×3 matrix has no LinearRaw meaning here).
    let mut found: Vec<(Illuminant, Vec<f32>)> = raw.color_matrix.iter().filter(|(_, m)| m.len() == 9).map(|(i, m)| (*i, m.clone())).collect();
    found.sort_by_key(|(i, _)| match i {
        Illuminant::A => 0,
        Illuminant::D65 => 2,
        Illuminant::D50 => 3,
        _ => 1,
    });
    out.matrix_count = 0;
    for (illuminant, m) in found.into_iter().take(2) {
        let slot = out.matrix_count as usize;
        out.illuminant[slot] = u16::from(illuminant);
        for k in 0..9 {
            out.matrix[slot][k] = m[k] as f64;
        }
        out.matrix_count += 1;
    }
    Some(())
}

/// The colour tags of a source DNG copied as they are (exact bytes of the values, no re-rounding).
const ROOT_COLOR_TAGS: &[DngTag] = &[
    DngTag::UniqueCameraModel,
    DngTag::LocalizedCameraModel,
    DngTag::CalibrationIlluminant1,
    DngTag::CalibrationIlluminant2,
    DngTag::ColorMatrix1,
    DngTag::ColorMatrix2,
    DngTag::CameraCalibration1,
    DngTag::CameraCalibration2,
    DngTag::ForwardMatrix1,
    DngTag::ForwardMatrix2,
    DngTag::AnalogBalance,
    DngTag::AsShotNeutral,
    DngTag::AsShotWhiteXY,
    DngTag::BaselineExposure,
    DngTag::BaselineExposureOffset,
    DngTag::BaselineNoise,
    DngTag::BaselineSharpness,
    DngTag::LinearResponseLimit,
    DngTag::CameraSerialNumber,
    DngTag::LensInfo,
    DngTag::CameraCalibrationSignature,
    DngTag::ProfileCalibrationSignature,
    DngTag::ProfileName,
    DngTag::ProfileEmbedPolicy,
    DngTag::ProfileCopyright,
    DngTag::ProfileHueSatMapDims,
    DngTag::ProfileHueSatMapData1,
    DngTag::ProfileHueSatMapData2,
    DngTag::ProfileHueSatMapEncoding,
    DngTag::ProfileLookTableDims,
    DngTag::ProfileLookTableData,
    DngTag::ProfileLookTableEncoding,
    DngTag::ProfileToneCurve,
    DngTag::DefaultBlackRender,
    DngTag::AsShotProfileName,
];
/// Per-raw-IFD colour tags some writers put next to the image instead of IFD 0 (the readers look there first).
const RAW_COLOR_TAGS: &[DngTag] = &[DngTag::CameraCalibration1, DngTag::CameraCalibration2, DngTag::ForwardMatrix1, DngTag::ForwardMatrix2];

fn write_impl(spec: &RawlerLinearDngSpec) -> Option<()> {
    let w = spec.width as usize;
    let h = spec.height as usize;
    if spec.rgb.is_null() || w == 0 || h == 0 || w > 65535 || h > 65535 {
        return None;
    }
    let rgb = unsafe { std::slice::from_raw_parts(spec.rgb, w * h * 3) };
    let out = path_from_c(spec.out_path)?;
    let source_path = path_from_c(spec.source_path);

    // rawler's view of the source: metadata (EXIF) and, for a DNG, its IFDs. Absent = the host's fallback fields.
    let source = source_path.as_ref().and_then(|p| RawSource::new(p).ok());
    let decoder = source.as_ref().and_then(|s| rawler::get_decoder(s).ok());
    let params = RawDecodeParams::default();
    let metadata = match (&decoder, &source) {
        (Some(d), Some(s)) => d.raw_metadata(s, &params).ok(),
        _ => None,
    };

    let file = File::create(&out).ok()?;
    let mut stream = BufWriter::new(file);
    let mut dng = DngWriter::new(&mut stream, DNG_VERSION_V1_4).ok()?;
    {
        // IFD 0 = the image (NewSubFileType 0): LinearRaw, BlackLevel 0, WhiteLevel 65535, LJ92 tiles, predictor 1.
        let mut sub = dng.subframe_on_root(0);
        sub.rgb_image_u16(rgb, w, h, DngCompression::Lossless, 1).ok()?;
        sub.finalize().ok()?;
    }
    if let Some(m) = &metadata {
        dng.load_metadata(m).ok()?;
    } else {
        let exif = dng.exif_ifd_mut();
        if spec.iso > 0.0 {
            exif.add_tag(ExifTag::ISOSpeedRatings, spec.iso.round().min(65535.0) as u16);
        }
        if spec.exposure_time > 0.0 {
            exif.add_tag(ExifTag::ExposureTime, rational(spec.exposure_time));
        }
        if spec.f_number > 0.0 {
            exif.add_tag(ExifTag::FNumber, rational(spec.f_number));
        }
        if spec.focal_length > 0.0 {
            exif.add_tag(ExifTag::FocalLength, rational(spec.focal_length));
        }
        let lens = cstr(spec.lens_model);
        if !lens.is_empty() {
            exif.add_tag(ExifTag::LensModel, lens.as_str());
        }
    }
    dng.exif_ifd_mut().remove_tag(ExifTag::MakerNotes);

    let root = dng.root_ifd_mut();
    // The colour model: a source DNG's own tags verbatim, else the host's.
    let mut copied = false;
    if spec.copy_source_dng_color != 0 {
        if let Some(d) = &decoder {
            if let Ok(Some(ifd)) = d.ifd(WellKnownIFD::Root) {
                for tag in ROOT_COLOR_TAGS {
                    if let Some(entry) = ifd.get_entry(*tag) {
                        root.add_value(*tag, entry.value.clone());
                    }
                }
            }
            if let Ok(Some(ifd)) = d.ifd(WellKnownIFD::Raw) {
                for tag in RAW_COLOR_TAGS {
                    if let (Some(entry), false) = (ifd.get_entry(*tag), root.contains(*tag)) {
                        root.add_value(*tag, entry.value.clone());
                    }
                }
            }
            copied = root.contains(DngTag::ColorMatrix1);
        }
    }
    if !copied {
        for tag in ROOT_COLOR_TAGS {
            root.remove_tag(*tag);
        }
        let n = spec.matrix_count.clamp(0, 2) as usize;
        for slot in 0..n {
            let m: Vec<SRational> = spec.matrix[slot].iter().map(|v| srational(*v)).collect();
            let (cal, mat) = if slot == 0 {
                (DngTag::CalibrationIlluminant1, DngTag::ColorMatrix1)
            } else {
                (DngTag::CalibrationIlluminant2, DngTag::ColorMatrix2)
            };
            root.add_tag(cal, spec.illuminant[slot]);
            root.add_tag(mat, m.as_slice());
        }
        if spec.baseline_exposure != 0.0 && spec.baseline_exposure.is_finite() {
            root.add_tag(DngTag::BaselineExposure, srational(spec.baseline_exposure));
        }
    }
    // As-shot white: the source DNG's own (copied) or the host's neutral; the writer's 1/1/1 never stays.
    if !(copied && (root.contains(DngTag::AsShotNeutral) || root.contains(DngTag::AsShotWhiteXY))) {
        root.remove_tag(DngTag::AsShotNeutral);
        if spec.has_neutral != 0 {
            let n: Vec<Rational> = spec.as_shot_neutral.iter().map(|v| rational(*v)).collect();
            root.add_tag(DngTag::AsShotNeutral, n.as_slice());
        }
    } else if root.contains(DngTag::AsShotWhiteXY) && !root.contains(DngTag::AsShotNeutral) {
        // DngWriter put (1, 1, 1) there; the source said white XY only.
        root.remove_tag(DngTag::AsShotNeutral);
    }
    let make = cstr(spec.make);
    let model = cstr(spec.model);
    if !make.is_empty() {
        root.add_tag(TiffCommonTag::Make, make.as_str());
    }
    if !model.is_empty() {
        root.add_tag(TiffCommonTag::Model, model.as_str());
    }
    if !root.contains(DngTag::UniqueCameraModel) {
        let ucm = cstr(spec.unique_camera_model);
        let ucm = if ucm.is_empty() { format!("{} {}", make, model).trim().to_string() } else { ucm };
        if !ucm.is_empty() {
            root.add_tag(DngTag::UniqueCameraModel, ucm.as_str());
        }
    }
    // The pedestal that keeps the camera's below-black noise (negative after black subtraction) instead of clamping it.
    root.add_tag(DngTag::BlackLevel, [spec.black_level, spec.black_level, spec.black_level]);
    root.add_tag(DngTag::DNGVersion, DNG_VERSION_V1_4);
    root.add_tag(DngTag::DNGBackwardVersion, DNG_VERSION_V1_4);
    root.add_tag(TiffCommonTag::Orientation, if (1..=8).contains(&spec.orientation) { spec.orientation } else { 1 });
    root.add_tag(DngTag::DefaultCropOrigin, [0_u32, 0]);
    root.add_tag(DngTag::DefaultCropSize, [w as u32, h as u32]);
    if spec.original_width > 0 && spec.original_height > 0 {
        let size = [spec.original_width, spec.original_height];
        root.add_tag(DngTag::OriginalDefaultFinalSize, size);
        root.add_tag(DngTag::OriginalBestQualityFinalSize, size);
        root.add_tag(DngTag::OriginalDefaultCropSize, size);
    }
    let software = cstr(spec.software);
    if !software.is_empty() {
        root.add_tag(TiffCommonTag::Software, software.as_str());
    }
    let xmp = cstr(spec.xmp_packet);
    if !xmp.is_empty() {
        dng.xpacket(xmp.as_bytes()).ok()?;
    }
    dng.close().ok()?;
    drop(stream);
    Some(())
}

/// See `include/rawler_ffi_proxy.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rawler_camera_color(path: *const c_char, out: *mut RawlerCameraColor) -> i32 {
    if out.is_null() {
        return 1;
    }
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut color: RawlerCameraColor = unsafe { std::mem::zeroed() };
        color.wb = [1.0; 3];
        let ok = path_from_c(path).and_then(|p| camera_color_impl(p, &mut color)).is_some();
        (ok, color)
    }));
    match result {
        Ok((ok, color)) => {
            unsafe { std::ptr::write(out, color) };
            if ok { 0 } else { 1 }
        }
        Err(_) => {
            unsafe { std::ptr::write(out, std::mem::zeroed()) };
            1
        }
    }
}

/// See `include/rawler_ffi_proxy.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rawler_linear_write_dng(spec: *const RawlerLinearDngSpec) -> i32 {
    if spec.is_null() {
        return 1;
    }
    let result = panic::catch_unwind(AssertUnwindSafe(|| write_impl(unsafe { &*spec })));
    match result {
        Ok(Some(())) => 0,
        _ => 1,
    }
}
