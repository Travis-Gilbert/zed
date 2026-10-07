#ifndef GPUI_NATIVE_GPU_FRAME_V1_H
#define GPUI_NATIVE_GPU_FRAME_V1_H

#include <stdint.h>
#include <stddef.h>

/* Same-process privileged host ABI. Storage lifetime is not browser authority.
 * Never serialize these pointers or expose them to a web page/MCP endpoint.
 * The producer dylib must stay loaded while any lease or callback is live.
 * Import validates metadata/version and actual IOSurface properties separately.
 */
typedef struct GpuiNativeGpuFrameMetadataV1 {
    uint32_t abi_version; /* 1 */
    uint32_t struct_size; /* sizeof(GpuiNativeGpuFrameMetadataV1) */
    uint32_t width;
    uint32_t height;
    uint64_t bytes_per_row;
    uint64_t source_registry;
    uint64_t source_surface;
    uint64_t source_resource_generation;
    uint64_t source_frame_generation;
    uint64_t transport_resource_generation;
    uint32_t pixel_format; /* FourCC BGRA, unorm8, top row first */
    uint32_t premultiplied_alpha; /* 1 */
} GpuiNativeGpuFrameMetadataV1;

typedef struct GpuiNativeGpuFrameLeaseV1 {
    GpuiNativeGpuFrameMetadataV1 metadata;
    const void *opaque;
    void (*retain)(const void *opaque);
    void (*release)(const void *opaque);
    const void *(*borrow_iosurface)(const void *opaque);
} GpuiNativeGpuFrameLeaseV1;

/* One returned struct owns one reference. Copies need retain before release of
 * the original. Finish all consumer GPU reads before releasing its reference.
 * borrow_iosurface returns a borrowed IOSurfaceRef, valid only with a live
 * lease. Never IOSurfaceLookup by global ID, and never share with a subprocess.
 */
#if defined(__cplusplus)
static_assert(sizeof(GpuiNativeGpuFrameMetadataV1) == 72, "metadata ABI changed");
#if UINTPTR_MAX == UINT64_MAX
static_assert(sizeof(GpuiNativeGpuFrameLeaseV1) == 104, "lease ABI changed");
#endif
#else
_Static_assert(sizeof(GpuiNativeGpuFrameMetadataV1) == 72, "metadata ABI changed");
#if UINTPTR_MAX == UINT64_MAX
_Static_assert(sizeof(GpuiNativeGpuFrameLeaseV1) == 104, "lease ABI changed");
#endif
#endif

#endif
