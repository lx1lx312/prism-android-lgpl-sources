// rawler_ffi_proxy.h — [Prism F2] the 2048 proxy DNG (src/proxy.rs): a LinearRaw DNG written with rawler's writer, and the
// camera's colour model as rawler knows it.
//
// The host reduces the sensor data to CAMERA RGB at the proxy's size (black → 0, white → 65535, before white balance and
// matrix, default crop applied, NOT oriented; SkiaRaw/IBRawProxyDNG.cpp) and calls rawler_linear_write_dng. The file:
// DNG 1.4, IFD 0 = the image, PhotometricInterpretation LinearRaw (34892), 3 × 16-bit unsigned samples, BlackLevel = the
// host's pedestal, WhiteLevel 65535, lossless JPEG (Compression 7, predictor 1) in 256 × 256 tiles. Every call is panic-safe.

#ifndef RAWLER_FFI_PROXY_H
#define RAWLER_FFI_PROXY_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct RawlerCameraColor {
    int32_t  matrix_count;     // 0…2 XYZ → camera 3×3 matrices rawler knows for the file
    uint16_t illuminant[2];    // their EXIF LightSource codes (17 = A, 21 = D65, …)
    double   matrix[2][9];     // row major (camera rows × XYZ)
    double   wb[3];            // rawler's as-shot multipliers R G B (1 when unknown)
    int32_t  is_dng;           // rawler reads the file as a DNG (the matrices are then the file's own tags)
    char     make[64];         // rawler's clean names, NUL-terminated
    char     model[64];
} RawlerCameraColor;

// The colour model of `path`: rawler's camera database for a non-DNG raw (the file's own matrix for a Hasselblad 3FR /
// FFF), the tags of a DNG. No sensor data is decoded. 0 = rawler read the file (matrix_count may still be 0: a camera the
// database does not know); 1 = rawler cannot read it. `out` is always written.
int32_t rawler_camera_color(const char *path, RawlerCameraColor *out);

typedef struct RawlerLinearDngSpec {
    const char     *out_path;            // written in place (the caller writes a temporary name, then renames)
    const char     *source_path;         // the original: EXIF from rawler's metadata, a DNG's colour tags (may be NULL)
    const uint16_t *rgb;                 // width × height × 3, interleaved camera RGB, 0…65535
    uint32_t        width, height;       // ≤ 65535
    uint16_t        orientation;         // EXIF 1…8 of the original (the pixels are NOT rotated)
    uint16_t        black_level;         // BlackLevel of all three samples (a pedestal for the below-black noise); WhiteLevel 65535
    int32_t         copy_source_dng_color;   // 1 = the source is a DNG: copy its colour / profile tags verbatim
    // Written when nothing was copied (a non-DNG source, or a DNG without ColorMatrix1):
    int32_t         matrix_count;        // 0…2
    uint16_t        illuminant[2];
    double          matrix[2][9];        // XYZ → camera, row major; SRATIONAL with a 1 / 1 000 000 denominator
    double          baseline_exposure;   // EV, 0 = no tag
    // As-shot white when the source DNG gave none (or nothing was copied): camera neutral, green = 1.
    int32_t         has_neutral;
    double          as_shot_neutral[3];
    const char     *make, *model;        // Make / Model (may be NULL = rawler's metadata or nothing)
    const char     *unique_camera_model; // when the source gave none; NULL / "" = "Make Model"
    uint32_t        original_width, original_height;   // the original's default-cropped size (Original*FinalSize / CropSize)
    // EXIF used only when rawler cannot read the source's metadata (0 / NULL = absent):
    double          iso, exposure_time, f_number, focal_length;
    const char     *lens_model;
    const char     *software;            // Software tag (may be NULL)
    const char     *xmp_packet;          // a complete XMP packet for ApplicationNotes (may be NULL)
} RawlerLinearDngSpec;

// 0 = written; 1 = failed (a partial file may exist).
int32_t rawler_linear_write_dng(const RawlerLinearDngSpec *spec);

#ifdef __cplusplus
}  // extern "C"
#endif

#endif  // RAWLER_FFI_PROXY_H
