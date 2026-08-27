# External GPU surface provenance

This implementation is based on Zed's GPUI source at commit
`8b1497dbd22fb06f5838a7c0b84a1e54fafa71bc` and is maintained on the
`theorem/external-gpu-surface` branch of Theorem's Zed fork.

The focused external-surface seam adapts architectural ideas observed in
[Far-Beyond-Pulsar/WGPUI](https://github.com/Far-Beyond-Pulsar/WGPUI) at commit
`41674d05a93648f7830a30a967bcae859f524291`, licensed under Apache-2.0. The
reference files were:

- `src/platform/cross/surface_registry.rs`
- `src/elements/wgpu_surface.rs`
- `src/platform/cross/renderer.rs`
- `src/platform/cross/shaders/surfaces.wgsl`
- `examples/learn/wgpu_surface_stress.rs`

The adapted concepts are a renderer-owned surface registry, a bounded
three-texture producer/ready/display rotation, serialized producer/compositor
submission, coalesced resize, and redraw invalidation on publication. The GPUI
API, target-specific platform adapters, scene integration, error model, and
implementation in this fork are a focused reimplementation for Theorem's
SceneOS boundary; WGPUI is not vendored and its broader API is not reproduced.

SceneOS remains outside GPUI. It consumes the narrow
`ExternalGpuSurfaceHandle` contract and submits ordinary `wgpu` command buffers;
GPUI remains responsible for layout, clipping, opacity, transformation, hit
testing, compositor ordering, and teardown.
