/* Blue Compressor core: re-encodes Nikon N-RAW (.NEV) and R3D NE (.R3D) TICO RAW clips.
 * Plain C API of libbluecompress (the Rust core built as a cdylib/staticlib), for build
 * servers, scripts and bindings in other languages. All paths are UTF-8.
 * SPDX-License-Identifier: Apache-2.0 */
#ifndef BLUECOMPRESS_H
#define BLUECOMPRESS_H

#include <stddef.h>
#include <stdint.h>

/* Define BC_SHARED_USE when linking the Windows DLL. */
#if defined(BC_SHARED_USE) && defined(_WIN32)
#  define BC_API __declspec(dllimport)
#else
#  define BC_API
#endif

#ifdef __cplusplus
extern "C" {
#endif

enum bc_resolution {
    BC_RES_ORIGINAL = 0, /* keep the recorded raw resolution */
    BC_RES_6K = 6,       /* 6048 x 3402 active (6060 x 3410 coded), only from larger sources */
    BC_RES_4K = 4        /* 3840 x 2160 active UHD (3852 x 2168 coded) */
};

/* Compression is set in bits per pixel (bits per Bayer sample of a frame) with
 * bc_transcode_bpp, or as the lossless repack. The quality steps are kept for existing
 * callers of bc_transcode: */
enum bc_quality {
    BC_Q1 = 1, /* lossless: camera's coded data repacked, pixel-identical (4 to 10% smaller) */
    BC_Q2 = 2, /* legacy: 0.42 x the clip's own bits per pixel */
    BC_Q3 = 3, /* legacy: 0.36 x the clip's own bits per pixel */
    BC_Q4 = 4  /* legacy: 0.24 x the clip's own bits per pixel */
};

enum bc_backend {
    BC_BACKEND_AUTO = 0,
    BC_BACKEND_CPU = 1,
    BC_BACKEND_METAL = 2,
    BC_BACKEND_CUDA = 3,
    BC_BACKEND_OPENCL = 4 /* AMD and other OpenCL GPUs */
};

typedef struct bc_clip_info {
    uint32_t width, height;     /* Bayer size */
    uint32_t frames;
    double fps;
    uint64_t file_bytes;
    uint64_t video_bytes;       /* video samples as stored, frame headers included */
    char codec[8];              /* "NRAW" (N-RAW) or "NR3D" (R3D NE) */
    int audio_tracks;
    uint64_t codestream_bytes;  /* TICO codestream bytes of the video frames */
} bc_clip_info;

typedef struct bc_options {
    const char* input;
    const char* output;         /* full output path */
    int quality;                /* bc_quality */
    int resolution;             /* bc_resolution */
    int backend;                /* bc_backend */
    int threads;                /* 0 = all cores */
} bc_options;

typedef struct bc_result {
    uint64_t input_bytes, output_bytes;
    uint32_t frames;
    double seconds;
    double fps;
    char backend[32];
    char error[512];
} bc_result;

/* progress: fraction 0..1, current frames per second. Return nonzero to cancel. */
typedef int (*bc_progress_fn)(void* user, double fraction, double fps);

BC_API int bc_probe(const char* path, bc_clip_info* info, char* error, size_t error_len);

/* Output resolution for a source and a requested resolution (0 if not applicable). */
BC_API void bc_output_size(uint32_t src_w, uint32_t src_h, int resolution, uint32_t* out_w, uint32_t* out_h);

/* Average bits per pixel of a probed clip: 8 x codestream_bytes / (frames x width x height)
 * (0 if it has no frames). */
BC_API double bc_clip_bpp(const bc_clip_info* info);

/* Runs synchronously; call from a background thread. Returns 0 on success. */
BC_API int bc_transcode(const bc_options* options, bc_progress_fn progress, void* user, bc_result* result);

/* bc_transcode to `bpp` bits per pixel (0.5 to 8, rounded to 0.1) instead of
 * options->quality. Each frame is rate controlled to fit; a target at or above the clip's
 * own rate, or bpp <= 0, gives the lossless repack. */
BC_API int bc_transcode_bpp(const bc_options* options, double bpp, bc_progress_fn progress, void* user, bc_result* result);

/* Names of the available GPU backends, e.g. "Metal (Apple GPU)". */
BC_API const char* bc_backend_description(int backend);

BC_API const char* bc_version(void);

#ifdef __cplusplus
}
#endif

#endif
