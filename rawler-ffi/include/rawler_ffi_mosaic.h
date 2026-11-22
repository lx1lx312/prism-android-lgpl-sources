// rawler_ffi_mosaic.h — [PR-MAC-172] the sensor mosaic of a RAW and a DNG writer for it (src/enhance.rs).
//
// Prism's Photo ▸ Enhance… (AI Denoise) reads the Bayer mosaic WITHOUT developing it, processes it, and writes the
// result as a new DNG (rawler's DNG writer, the DngLab conversion: same levels, CFA, colour matrices,
// AsShotNeutral, EXIF, embedded XMP + prism:EnhancedFrom, the original's preview).
//
// Only 2×2 Bayer sensors with integer samples open (X-Trans, LinearRaw / float DNGs and the rotated Fujifilm
// SuperCCD sensors answer RAWLER_MOSAIC_NOT_BAYER / _UNSUPPORTED_DATA). Every call is panic-safe.

#ifndef RAWLER_FFI_MOSAIC_H
#define RAWLER_FFI_MOSAIC_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

enum {
    RAWLER_MOSAIC_OK = 0,
    RAWLER_MOSAIC_DECODE_FAILED = 1,
    RAWLER_MOSAIC_NOT_BAYER = 2,
    RAWLER_MOSAIC_UNSUPPORTED_DATA = 3,
};

typedef struct RawlerMosaicInfo {
    int32_t width, height;          // the full sensor frame (masked borders included)
    int32_t cfa[4];                 // colour at (0,0) (0,1) (1,0) (1,1): 0 R, 1 G, 2 B
    float   black[4], white[4];     // levels at the same four positions, sensor units
    int32_t active_x, active_y, active_w, active_h;
    float   iso;                    // 0 when unknown
    float   wb[4];                  // as-shot white balance multipliers R G B (E); 1 when unknown
    int32_t orientation;            // EXIF orientation 1…8 of the photo (the mosaic is NOT rotated)
    char    camera_make[64];
    char    camera_model[64];
} RawlerMosaicInfo;

typedef struct RawlerMosaic RawlerMosaic;

// Decodes the sensor data of `path`. `out_info` is filled as far as the decode got (also on failure: make, model,
// size); `out_status` gets a RAWLER_MOSAIC_* code. NULL unless RAWLER_MOSAIC_OK. Free with rawler_mosaic_free.
RawlerMosaic *rawler_mosaic_open(const char *path, RawlerMosaicInfo *out_info, int32_t *out_status);

// width × height f32, (value − black) / (white − black) per CFA position, NOT clipped. Owned by the handle.
const float *rawler_mosaic_pixels(const RawlerMosaic *m);

// Writes `pixels` (same layout and normalisation) as a DNG 1.4 (lossless JPEG) at `out_path`. `software` goes in
// the Software tag, `enhanced_from` (may be NULL / empty) in the XMP as prism:EnhancedFrom.
// 0 = written, 1 = failed (a partial file may exist: write to a temporary name, then rename).
int32_t rawler_mosaic_write_dng(const RawlerMosaic *m, const float *pixels, const char *out_path,
                                const char *software, const char *enhanced_from);

void rawler_mosaic_free(RawlerMosaic *m);

#ifdef __cplusplus
}  // extern "C"
#endif

#endif  // RAWLER_FFI_MOSAIC_H
