// rawler_ffi.h — C ABI for the rawler-ffi static library.
//
// Camera RAW loader shared by ImageBrowser and (later) Photo Editor Pro,
// built on rawler — the pure-Rust RAW decoder used by RapidRAW (DngLab).
// Cross-platform by design: this exact ABI must compile on macOS and Windows.
//
// The library is built by build.sh into lib/librawler_ffi.a (arm64 macOS for
// now, mirroring ThirdParty/skia-metal which is arm64-only).
//
// Pixel contract: rawler_decode_path returns LINEAR (scene-referred) RGB,
// f32 interleaved, orientation ALREADY APPLIED (pixels physically rotated,
// width/height are post-rotation). No sRGB curve is baked in — the editing
// shader applies exposure/WB/tone in linear light and encodes sRGB at the end.
//
// The colour stage — clip test, highlight reconstruction, white balance, camera
// matrix, gamut map — belongs to this library and not to rawler (see
// `HighlightRecon` in src/lib.rs for why). Two consequences for the caller:
// values above 1.0 are real highlights and must be tone-mapped, not clamped;
// and every value is >= 0, already folded inside the sRGB gamut by desaturation
// rather than by a per-channel clamp, so no caller needs to clip negatives.

#ifndef RAWLER_FFI_H
#define RAWLER_FFI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct RawlerMeta {
    int32_t orientation;        // EXIF orientation as decoded (informational; pixels are already rotated)
    float   iso;                // 0 when unknown
    float   exposure_time;      // seconds, 0 when unknown
    float   aperture;           // f-number, 0 when unknown
    float   focal_length;       // mm, 0 when unknown
    float   wb_r, wb_g, wb_b;   // as-shot white balance multipliers (1,1,1 when unknown)
    char    camera_make[64];    // NUL-terminated UTF-8, possibly truncated
    char    camera_model[64];   // NUL-terminated UTF-8, possibly truncated
} RawlerMeta;

typedef struct RawlerImage {
    int32_t     width;          // post-orientation
    int32_t     height;         // post-orientation
    int32_t     channels;       // always 3 (RGB)
    float      *pixels;         // width*height*3 f32, linear RGB in [0,1] (highlights may exceed 1)
    RawlerMeta  meta;
} RawlerImage;

// Decode + develop a RAW file (CR2/CR3/NEF/ARW/RAF/DNG/…) at `path`.
// half_size != 0 selects the fast demosaic path (roughly half resolution),
// for interactive previews. Returns NULL on any failure (unsupported file,
// I/O error, decoder panic — the library never unwinds across the ABI).
// The returned image is heap-allocated and OWNED BY THE CALLER: free it with
// rawler_free_image.
RawlerImage *rawler_decode_path(const char *path, int32_t half_size);

// [hotpixels] rawler_decode_path with options. RAWLER_DECODE_HOT_PIXELS suppresses the hot photosites ON THE MOSAIC,
// before the demosaic (src/hotpixels.rs): a photosite brighter than every same-colour neighbour of its 5x5 window,
// above their median by more than k * sigma (sigma = 1.4826 * MAD of those neighbours), and isolated (the
// orthogonal neighbours of the other colours do not rise with it, so stars stay) is replaced by that median.
// `options` NULL = rawler_decode_path(path, 0). `stats` (may be NULL) is filled on return, also on failure.
enum { RAWLER_DECODE_HOT_PIXELS = 1u };

typedef struct RawlerDecodeOptions {
    int32_t  half_size;      // as rawler_decode_path
    uint32_t flags;          // RAWLER_DECODE_*
    float    hot_pixel_k;    // threshold in sigma; <= 0 = the default (7)
} RawlerDecodeOptions;

typedef struct RawlerDecodeStats {
    uint64_t hot_pixels_replaced;
    uint64_t hot_pixels_examined;   // photosites of the active area that were tested
    double   hot_pixels_ms;         // wall time of the stage
    int32_t  hot_pixels_ran;        // 1 = the stage ran (a CFA mosaic); 0 = not asked / not a mosaic
} RawlerDecodeStats;

RawlerImage *rawler_decode_path_ex(const char *path, const RawlerDecodeOptions *options, RawlerDecodeStats *stats);

// Extract the largest embedded preview JPEG without decoding the sensor data
// (instant thumbnails). On success returns a heap-allocated JPEG byte buffer
// OWNED BY THE CALLER (free with rawler_free_bytes) and writes its length to
// *out_len. Returns NULL when the file has no usable embedded preview.
uint8_t *rawler_thumbnail_jpeg(const char *path, size_t *out_len);

// Returns 1 when rawler recognizes the file at `path` as a RAW it can decode,
// 0 otherwise. Cheap (header sniff, no full decode).
int32_t rawler_is_supported(const char *path);

// Free an image returned by rawler_decode_path. NULL is a no-op.
void rawler_free_image(RawlerImage *img);

// Free a buffer returned by rawler_thumbnail_jpeg. NULL is a no-op.
void rawler_free_bytes(uint8_t *p);

#ifdef __cplusplus
}  // extern "C"
#endif

#endif  // RAWLER_FFI_H
