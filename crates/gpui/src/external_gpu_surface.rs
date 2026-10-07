// Theorem modification: validate compositor ownership before scene export.
use parking_lot::{Mutex, MutexGuard};
use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

#[cfg(target_family = "wasm")]
use std::rc::Rc;

/// An error returned by GPUI's external GPU surface seam.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ExternalGpuSurfaceError {
    /// The active platform renderer cannot composite external GPU textures.
    #[error("the active GPUI platform does not support external GPU surfaces")]
    Unsupported,
    /// The requested texture format is not a supported color surface format.
    #[error("texture format {0:?} is not supported for an external GPU surface")]
    UnsupportedFormat(wgpu::TextureFormat),
    /// The renderer's device was lost. Create a new surface after renderer recovery.
    #[error("the external GPU surface device was lost")]
    DeviceLost,
    /// The surface was removed from its compositor registry.
    #[error("the external GPU surface is no longer registered")]
    Closed,
    /// The requested dimensions exceed this device's texture limits.
    #[error("external GPU surface dimensions {width}x{height} exceed limit {limit}")]
    InvalidSize {
        /// Requested device-pixel width.
        width: u32,
        /// Requested device-pixel height.
        height: u32,
        /// Device's maximum two-dimensional texture extent.
        limit: u32,
    },
    /// A pending resize no longer matches the scene being exported.
    #[error("external GPU surface dimensions changed from {expected:?} to {actual:?}")]
    SizeChanged {
        /// Device-pixel viewport used to build the scene.
        expected: (u32, u32),
        /// Current or pending device-pixel surface extent.
        actual: (u32, u32),
    },
}

/// The terminal-aware state of an external GPU surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalGpuSurfaceStatus {
    /// The surface can acquire, render, and present frames.
    Ready,
    /// The creating device was lost; this handle cannot be recovered in place.
    DeviceLost,
    /// The compositor no longer owns this surface.
    Closed,
}

/// Opaque identity for an external GPU surface in a compositor registry.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ExternalGpuSurfaceId {
    registry: u64,
    surface: u64,
}

// A scene can contain handles from multiple windows. Registry-local counters
// alone would let a foreign handle alias a healthy compositor-owned surface.
static NEXT_REGISTRY_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(not(target_family = "wasm"))]
#[doc(hidden)]
pub type ExternalGpuSurfaceInvalidator = Arc<dyn Fn() + Send + Sync>;

#[cfg(target_family = "wasm")]
#[doc(hidden)]
pub type ExternalGpuSurfaceInvalidator = Rc<dyn Fn()>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BufferIndices {
    rendering: usize,
    ready: usize,
    display: usize,
}

impl Default for BufferIndices {
    fn default() -> Self {
        Self {
            rendering: 0,
            ready: 1,
            display: 2,
        }
    }
}

impl BufferIndices {
    fn publish(&mut self) -> usize {
        let published = self.rendering;
        std::mem::swap(&mut self.rendering, &mut self.ready);
        published
    }

    fn promote(&mut self) {
        std::mem::swap(&mut self.ready, &mut self.display);
    }

    #[cfg(test)]
    fn are_distinct(self) -> bool {
        self.rendering != self.ready && self.ready != self.display && self.display != self.rendering
    }
}

struct TripleBuffer {
    textures: [wgpu::Texture; 3],
    views: [wgpu::TextureView; 3],
    indices: BufferIndices,
    submission_indices: [Option<wgpu::SubmissionIndex>; 3],
    premultiplied_alpha: [bool; 3],
    frame_generation: u64,
    composited_generation: u64,
    redraw_pending: bool,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
}

impl TripleBuffer {
    fn new(
        device: &wgpu::Device,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> Result<Self, ExternalGpuSurfaceError> {
        if !matches!(
            format,
            wgpu::TextureFormat::Rgba8Unorm
                | wgpu::TextureFormat::Rgba8UnormSrgb
                | wgpu::TextureFormat::Bgra8Unorm
                | wgpu::TextureFormat::Bgra8UnormSrgb
        ) {
            return Err(ExternalGpuSurfaceError::UnsupportedFormat(format));
        }

        let width = width.max(1);
        let height = height.max(1);
        let limit = device.limits().max_texture_dimension_2d;
        if width > limit || height > limit {
            return Err(ExternalGpuSurfaceError::InvalidSize {
                width,
                height,
                limit,
            });
        }
        let create_texture = |label| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };

        let textures = [
            create_texture("gpui_external_surface_0"),
            create_texture("gpui_external_surface_1"),
            create_texture("gpui_external_surface_2"),
        ];
        let views = textures
            .each_ref()
            .map(|texture| texture.create_view(&wgpu::TextureViewDescriptor::default()));

        Ok(Self {
            textures,
            views,
            indices: BufferIndices::default(),
            submission_indices: [None, None, None],
            premultiplied_alpha: [false; 3],
            frame_generation: 0,
            composited_generation: 0,
            redraw_pending: false,
            width,
            height,
            format,
        })
    }

    fn allocated_bytes(&self) -> u64 {
        let bytes_per_texel = u64::from(self.format.block_copy_size(None).unwrap_or(0));
        u64::from(self.width)
            .saturating_mul(u64::from(self.height))
            .saturating_mul(bytes_per_texel)
            .saturating_mul(self.textures.len() as u64)
    }
}

/// The compositor-owned registry behind [`ExternalGpuSurfaceHandle`].
///
/// This type is public only so platform renderer crates can share it with `gpui`.
#[doc(hidden)]
pub struct ExternalGpuSurfaceRegistry {
    surfaces: Mutex<HashMap<ExternalGpuSurfaceId, TripleBuffer>>,
    registry_id: u64,
    next_id: AtomicU64,
    submission_gate: Mutex<()>,
}

impl Default for ExternalGpuSurfaceRegistry {
    fn default() -> Self {
        Self {
            surfaces: Mutex::new(HashMap::new()),
            registry_id: NEXT_REGISTRY_ID
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .expect("GPU surface registry identity exhausted"),
            next_id: AtomicU64::new(1),
            submission_gate: Mutex::new(()),
        }
    }
}

impl ExternalGpuSurfaceRegistry {
    /// Create an empty registry.
    #[doc(hidden)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Serialize compositor submission with producer submission and resize.
    #[doc(hidden)]
    pub fn lock_submission(&self) -> MutexGuard<'_, ()> {
        self.submission_gate.lock()
    }

    /// Allocate exactly three textures and register them as one surface.
    #[doc(hidden)]
    pub fn register(
        &self,
        device: &wgpu::Device,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> Result<ExternalGpuSurfaceId, ExternalGpuSurfaceError> {
        let id = self.next_surface_id();
        let triple_buffer = TripleBuffer::new(device, width, height, format)?;
        self.surfaces.lock().insert(id, triple_buffer);
        Ok(id)
    }

    fn next_surface_id(&self) -> ExternalGpuSurfaceId {
        ExternalGpuSurfaceId {
            registry: self.registry_id,
            surface: self
                .next_id
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .expect("GPU surface identity exhausted"),
        }
    }

    fn remove(&self, id: ExternalGpuSurfaceId) {
        self.surfaces.lock().remove(&id);
    }

    fn contains(&self, id: ExternalGpuSurfaceId) -> bool {
        self.surfaces.lock().contains_key(&id)
    }

    fn back_view_with_size(
        &self,
        id: ExternalGpuSurfaceId,
    ) -> Option<(wgpu::TextureView, (u32, u32))> {
        self.surfaces.lock().get(&id).map(|surface| {
            (
                surface.views[surface.indices.rendering].clone(),
                (surface.width, surface.height),
            )
        })
    }

    fn publish(
        &self,
        id: ExternalGpuSurfaceId,
        submission_index: wgpu::SubmissionIndex,
        premultiplied_alpha: bool,
    ) -> Option<bool> {
        let mut surfaces = self.surfaces.lock();
        let Some(surface) = surfaces.get_mut(&id) else {
            return None;
        };
        let published = surface.indices.publish();
        surface.submission_indices[published] = Some(submission_index);
        surface.premultiplied_alpha[published] = premultiplied_alpha;
        surface.frame_generation = surface.frame_generation.wrapping_add(1);
        let should_invalidate = !surface.redraw_pending;
        surface.redraw_pending = true;
        Some(should_invalidate)
    }

    /// Promote the newest producer frame and return the stable display view.
    #[doc(hidden)]
    pub fn promote_and_front_view(&self, id: ExternalGpuSurfaceId) -> Option<wgpu::TextureView> {
        self.promote_and_front_view_with_alpha(id)
            .map(|(view, _)| view)
    }

    /// Promote color data and its alpha representation as one frame.
    #[doc(hidden)]
    pub fn promote_and_front_view_with_alpha(
        &self,
        id: ExternalGpuSurfaceId,
    ) -> Option<(wgpu::TextureView, bool)> {
        let mut surfaces = self.surfaces.lock();
        let surface = surfaces.get_mut(&id)?;
        if surface.frame_generation != surface.composited_generation {
            surface.indices.promote();
            surface.composited_generation = surface.frame_generation;
        }
        surface.redraw_pending = false;
        Some((
            surface.views[surface.indices.display].clone(),
            surface.premultiplied_alpha[surface.indices.display],
        ))
    }

    fn resize(
        &self,
        device: &wgpu::Device,
        id: ExternalGpuSurfaceId,
        width: u32,
        height: u32,
    ) -> Result<(u32, u32), ExternalGpuSurfaceError> {
        let mut surfaces = self.surfaces.lock();
        let surface = surfaces
            .get_mut(&id)
            .ok_or(ExternalGpuSurfaceError::Closed)?;
        let width = width.max(1);
        let height = height.max(1);
        if (surface.width, surface.height) == (width, height) {
            return Ok((width, height));
        }
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .map_err(|_| ExternalGpuSurfaceError::DeviceLost)?;
        *surface = TripleBuffer::new(device, width, height, surface.format)?;
        Ok((width, height))
    }

    fn size(&self, id: ExternalGpuSurfaceId) -> Option<(u32, u32)> {
        self.surfaces
            .lock()
            .get(&id)
            .map(|surface| (surface.width, surface.height))
    }

    fn allocated_bytes(&self, id: ExternalGpuSurfaceId) -> Option<u64> {
        self.surfaces
            .lock()
            .get(&id)
            .map(TripleBuffer::allocated_bytes)
    }

    fn has_unconsumed_frame(&self, id: ExternalGpuSurfaceId) -> Option<bool> {
        self.surfaces
            .lock()
            .get(&id)
            .map(|surface| surface.frame_generation != surface.composited_generation)
    }
}

struct ExternalGpuSurfaceInner {
    id: ExternalGpuSurfaceId,
    registry: Arc<ExternalGpuSurfaceRegistry>,
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    format: wgpu::TextureFormat,
    pending_size: Mutex<Option<(u32, u32)>>,
    device_lost: Arc<AtomicBool>,
    invalidator: ExternalGpuSurfaceInvalidator,
}

impl Drop for ExternalGpuSurfaceInner {
    fn drop(&mut self) {
        let _submission_guard = self.registry.lock_submission();
        self.registry.remove(self.id);
    }
}

/// A cloneable handle to a bounded, compositor-owned GPU texture surface.
///
/// Acquire a frame with [`Self::acquire_frame`], encode commands against its
/// texture view, and finish with [`ExternalGpuSurfaceFrame::submit_and_present`].
/// GPUI then samples the newest completed slot as an ordinary scene primitive.
#[derive(Clone)]
pub struct ExternalGpuSurfaceHandle {
    inner: Arc<ExternalGpuSurfaceInner>,
}

impl ExternalGpuSurfaceHandle {
    /// Whether this handle belongs to exactly this compositor registry and GPU.
    ///
    /// Registry-local surface IDs alone cannot identify a resource across renderers.
    #[doc(hidden)]
    pub fn belongs_to(
        &self,
        registry: &Arc<ExternalGpuSurfaceRegistry>,
        device: &Arc<wgpu::Device>,
        queue: &Arc<wgpu::Queue>,
    ) -> bool {
        Arc::ptr_eq(&self.inner.registry, registry)
            && Arc::ptr_eq(&self.inner.device, device)
            && Arc::ptr_eq(&self.inner.queue, queue)
    }

    /// Construct a handle from a platform renderer's shared registry.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Arc<ExternalGpuSurfaceRegistry>,
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
        device_lost: Arc<AtomicBool>,
        invalidator: ExternalGpuSurfaceInvalidator,
    ) -> Result<Self, ExternalGpuSurfaceError> {
        let id = {
            let _submission_guard = registry.lock_submission();
            registry.register(&device, width, height, format)?
        };
        Ok(Self {
            inner: Arc::new(ExternalGpuSurfaceInner {
                id,
                registry,
                device,
                queue,
                format,
                pending_size: Mutex::new(None),
                device_lost,
                invalidator,
            }),
        })
    }

    /// Return the renderer-owned device used by both producer and compositor.
    pub fn device(&self) -> &wgpu::Device {
        &self.inner.device
    }

    /// Return the renderer-owned queue used by both producer and compositor.
    pub fn queue(&self) -> &wgpu::Queue {
        &self.inner.queue
    }

    /// Return the surface texture format.
    pub fn format(&self) -> wgpu::TextureFormat {
        self.inner.format
    }

    /// Return the currently committed size in device pixels.
    pub fn size(&self) -> Result<(u32, u32), ExternalGpuSurfaceError> {
        self.inner
            .registry
            .size(self.inner.id)
            .ok_or(ExternalGpuSurfaceError::Closed)
    }

    /// Coalesce a resize request. The newest request is applied at frame acquisition.
    pub fn request_resize(&self, width: u32, height: u32) {
        let requested_size = (width.max(1), height.max(1));
        let should_invalidate = {
            let mut pending_size = self.inner.pending_size.lock();
            if *pending_size == Some(requested_size) {
                false
            } else {
                *pending_size = Some(requested_size);
                true
            }
        };
        if should_invalidate {
            (self.inner.invalidator)();
        }
    }

    /// Return whether a resize is waiting to be applied.
    pub fn is_resize_pending(&self) -> bool {
        self.inner.pending_size.lock().is_some()
    }

    /// Return whether a published frame has not yet been promoted by GPUI.
    ///
    /// Producers can use this as backpressure and skip work that would only
    /// replace the already-bounded ready slot.
    pub fn has_unconsumed_frame(&self) -> Result<bool, ExternalGpuSurfaceError> {
        self.inner
            .registry
            .has_unconsumed_frame(self.inner.id)
            .ok_or(ExternalGpuSurfaceError::Closed)
    }

    /// Return the surface status, including the typed terminal device-loss state.
    pub fn status(&self) -> ExternalGpuSurfaceStatus {
        if self.inner.device_lost.load(Ordering::Acquire) {
            ExternalGpuSurfaceStatus::DeviceLost
        } else if self.inner.registry.contains(self.inner.id) {
            ExternalGpuSurfaceStatus::Ready
        } else {
            ExternalGpuSurfaceStatus::Closed
        }
    }

    /// Acquire the rendering slot while excluding compositor submission and resize.
    pub fn acquire_frame(&self) -> Result<ExternalGpuSurfaceFrame<'_>, ExternalGpuSurfaceError> {
        self.acquire_frame_for_size(None)
    }

    /// Acquire only if the current or pending size matches the scene's viewport.
    /// A mismatch refuses before allocating a pending resize.
    pub fn acquire_frame_at_size(
        &self,
        expected: (u32, u32),
    ) -> Result<ExternalGpuSurfaceFrame<'_>, ExternalGpuSurfaceError> {
        self.acquire_frame_for_size(Some(expected))
    }

    fn acquire_frame_for_size(
        &self,
        expected: Option<(u32, u32)>,
    ) -> Result<ExternalGpuSurfaceFrame<'_>, ExternalGpuSurfaceError> {
        match self.status() {
            ExternalGpuSurfaceStatus::Ready => {}
            ExternalGpuSurfaceStatus::DeviceLost => {
                return Err(ExternalGpuSurfaceError::DeviceLost);
            }
            ExternalGpuSurfaceStatus::Closed => return Err(ExternalGpuSurfaceError::Closed),
        }

        let submission_guard = self.inner.registry.lock_submission();
        let mut pending_size = self.inner.pending_size.lock();
        let size = pending_size.unwrap_or(self.size()?);
        let size = (size.0.max(1), size.1.max(1));
        if let Some(expected) = expected
            && expected != size
        {
            return Err(ExternalGpuSurfaceError::SizeChanged {
                expected,
                actual: size,
            });
        }
        let limit = self.inner.device.limits().max_texture_dimension_2d;
        if size.0 > limit || size.1 > limit {
            return Err(ExternalGpuSurfaceError::InvalidSize {
                width: size.0,
                height: size.1,
                limit,
            });
        }
        if pending_size.take().is_some() {
            self.inner
                .registry
                .resize(&self.inner.device, self.inner.id, size.0, size.1)?;
        }
        drop(pending_size);
        let (view, size) = self
            .inner
            .registry
            .back_view_with_size(self.inner.id)
            .ok_or(ExternalGpuSurfaceError::Closed)?;

        Ok(ExternalGpuSurfaceFrame {
            handle: self,
            view,
            size,
            submission_guard: Some(submission_guard),
        })
    }

    /// Return the exact memory occupied by this surface's three color textures.
    pub fn allocated_texture_bytes(&self) -> Result<u64, ExternalGpuSurfaceError> {
        self.inner
            .registry
            .allocated_bytes(self.inner.id)
            .ok_or(ExternalGpuSurfaceError::Closed)
    }

    /// Return the opaque registry identity used by the GPUI scene primitive.
    #[doc(hidden)]
    pub fn id(&self) -> ExternalGpuSurfaceId {
        self.inner.id
    }
}

impl fmt::Debug for ExternalGpuSurfaceHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExternalGpuSurfaceHandle")
            .field("id", &self.inner.id)
            .field("format", &self.inner.format)
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl PartialEq for ExternalGpuSurfaceHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for ExternalGpuSurfaceHandle {}

/// One exclusively synchronized frame acquired from an external GPU surface.
pub struct ExternalGpuSurfaceFrame<'a> {
    handle: &'a ExternalGpuSurfaceHandle,
    view: wgpu::TextureView,
    size: (u32, u32),
    submission_guard: Option<MutexGuard<'a, ()>>,
}

impl ExternalGpuSurfaceFrame<'_> {
    /// Return the render-target view for this frame.
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// Return this frame's immutable device-pixel dimensions.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Submit command buffers and atomically publish this frame to GPUI.
    pub fn submit_and_present(
        self,
        command_buffers: impl IntoIterator<Item = wgpu::CommandBuffer>,
    ) -> Result<wgpu::SubmissionIndex, ExternalGpuSurfaceError> {
        self.submit_with_alpha(command_buffers, false)
    }

    /// Publish a scene-rendered frame whose stored RGB already includes alpha.
    pub fn submit_premultiplied_and_present(
        self,
        command_buffers: impl IntoIterator<Item = wgpu::CommandBuffer>,
    ) -> Result<wgpu::SubmissionIndex, ExternalGpuSurfaceError> {
        self.submit_with_alpha(command_buffers, true)
    }

    fn submit_with_alpha(
        mut self,
        command_buffers: impl IntoIterator<Item = wgpu::CommandBuffer>,
        premultiplied_alpha: bool,
    ) -> Result<wgpu::SubmissionIndex, ExternalGpuSurfaceError> {
        if self.handle.status() == ExternalGpuSurfaceStatus::DeviceLost {
            return Err(ExternalGpuSurfaceError::DeviceLost);
        }
        let submission_index = self.handle.inner.queue.submit(command_buffers);
        let should_invalidate = self
            .handle
            .inner
            .registry
            .publish(
                self.handle.inner.id,
                submission_index.clone(),
                premultiplied_alpha,
            )
            .ok_or(ExternalGpuSurfaceError::Closed)?;
        drop(self.submission_guard.take());
        if should_invalidate {
            (self.handle.inner.invalidator)();
        }
        Ok(submission_index)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BufferIndices, ExternalGpuSurfaceError, ExternalGpuSurfaceHandle,
        ExternalGpuSurfaceRegistry, ExternalGpuSurfaceStatus,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };

    #[test]
    fn external_gpu_surface_identity_is_qualified_by_registry() {
        let first = ExternalGpuSurfaceRegistry::new();
        let second = ExternalGpuSurfaceRegistry::new();
        let a = first.next_surface_id();
        let b = second.next_surface_id();
        assert_eq!(
            a.surface, b.surface,
            "both registries begin at the same local slot"
        );
        assert_ne!(
            a, b,
            "foreign surfaces must never alias local samples or failures"
        );
        assert_ne!(a, first.next_surface_id());
    }

    #[test]
    fn external_gpu_surface_triple_buffer_rotation_stays_distinct() {
        let mut indices = BufferIndices::default();
        for _ in 0..1_000 {
            indices.publish();
            assert!(indices.are_distinct());
            indices.promote();
            assert!(indices.are_distinct());
        }
    }

    #[test]
    fn external_gpu_surface_skipped_composite_keeps_display_stable() {
        let mut indices = BufferIndices::default();
        let initial_display = indices.display;
        indices.publish();
        assert_eq!(indices.display, initial_display);
        indices.promote();
        assert_ne!(indices.display, initial_display);
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    #[ignore = "requires a live native wgpu adapter"]
    fn external_gpu_surface_live_submit_resize_stress_is_bounded() -> anyhow::Result<()> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            flags: wgpu::InstanceFlags::default(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display: None,
        });
        let adapter = crate::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))?;
        let info = adapter.get_info();
        println!(
            "external surface adapter: {:?}, {:?}, {}",
            info.backend, info.device_type, info.name
        );
        let (device, queue) = crate::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("external_gpu_surface_live_test_device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults()
                    .using_resolution(adapter.limits())
                    .using_alignment(adapter.limits()),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                trace: wgpu::Trace::Off,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
            }),
        )?;
        let device = Arc::new(device);
        let queue = Arc::new(queue);
        let registry = Arc::new(ExternalGpuSurfaceRegistry::new());
        let device_lost = Arc::new(AtomicBool::new(false));
        let invalidations = Arc::new(AtomicU64::new(0));
        let invalidator = {
            let invalidations = Arc::clone(&invalidations);
            Arc::new(move || {
                invalidations.fetch_add(1, Ordering::Relaxed);
            })
        };
        let handle = ExternalGpuSurfaceHandle::new(
            Arc::clone(&registry),
            Arc::clone(&device),
            Arc::clone(&queue),
            64,
            48,
            wgpu::TextureFormat::Rgba8Unorm,
            Arc::clone(&device_lost),
            invalidator,
        )?;

        assert!(handle.belongs_to(&registry, &device, &queue));
        assert!(!handle.belongs_to(
            &Arc::new(ExternalGpuSurfaceRegistry::new()),
            &device,
            &queue
        ));
        handle.request_resize(device.limits().max_texture_dimension_2d + 1, 48);
        assert!(matches!(
            handle.acquire_frame_at_size((64, 48)),
            Err(ExternalGpuSurfaceError::SizeChanged { .. })
        ));
        assert!(matches!(
            handle.acquire_frame(),
            Err(ExternalGpuSurfaceError::InvalidSize { .. })
        ));
        assert_eq!(handle.size()?, (64, 48));

        for iteration in 0..256_u32 {
            let width = 64 + iteration % 17;
            let height = 48 + iteration % 13;
            handle.request_resize(width, height);
            let frame = handle.acquire_frame()?;
            assert_eq!(frame.size(), (width, height));
            let encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("external_gpu_surface_live_test_encoder"),
            });
            if iteration % 2 == 0 {
                frame.submit_premultiplied_and_present([encoder.finish()])?;
            } else {
                frame.submit_and_present([encoder.finish()])?;
            }
            assert!(handle.has_unconsumed_frame()?);
            let (_, premultiplied_alpha) = registry
                .promote_and_front_view_with_alpha(handle.id())
                .expect("published frame");
            assert_eq!(premultiplied_alpha, iteration % 2 == 0);
            assert!(!handle.has_unconsumed_frame()?);
            assert_eq!(
                handle.allocated_texture_bytes()?,
                u64::from(width) * u64::from(height) * 4 * 3
            );
        }

        assert_eq!(handle.status(), ExternalGpuSurfaceStatus::Ready);
        assert!(invalidations.load(Ordering::Relaxed) >= 256);
        device_lost.store(true, Ordering::Release);
        assert_eq!(handle.status(), ExternalGpuSurfaceStatus::DeviceLost);
        assert!(matches!(
            handle.acquire_frame(),
            Err(ExternalGpuSurfaceError::DeviceLost)
        ));
        let id = handle.id();
        drop(handle);
        assert!(registry.promote_and_front_view(id).is_none());
        Ok(())
    }
}
