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


## Retained capture adaptation, 2026-10-07

The paint-only retained capture scope adapts core Apache-2.0 mechanisms from
[cradiy/gpui-fusion](https://github.com/cradiy/gpui-fusion) at
`2f0e58241e4dba0df582d54a8f54d28c355c1781`: `crates/gpui/src/scene.rs`,
`crates/gpui/src/window/effects.rs`, `crates/gpui/src/view.rs`, and the sizing
contract in `crates/gpui/src/scene3d.rs`. Attribution includes gpui-fusion
contributors and the original GPUI/Zed contributors. The existing
`LICENSE-APACHE` applies. No donor `gpui_3d` code, MIT assets, new device,
renderer distribution, or semantic scene owner is imported.

Theorem's adaptation records nested capture boundaries in the replay stream,
retains the supplied element/entity, scopes source-local masks, offsets, viewport
and bounded raster density, and disables interactive/cache/overlay reuse in the
capture. Window state is restored on unwind. Publication uses the existing
compositor's GPU lease exporter; unsupported platforms refuse explicitly. Failed
leases are excluded from window composition and reported to the trusted host,
while ordinary window paint remains available. Surface identities include a
non-reused process registry token, so a refused foreign lease cannot alias the
local compositor's first slot. Captured text uses grayscale coverage because
subpixel coverage cannot be transferred to a transparent transformed target.

This API is paint-only. A Theorem-owned adapter must still establish provider
and presentation generation ownership, pointer mapping, keyboard focus, IME,
accessibility projection, overlay placement, resource recovery, trusted return,
and any privileged browser GPU import. A captured interface must not be
advertised as interactive before those obligations are implemented and proved.
