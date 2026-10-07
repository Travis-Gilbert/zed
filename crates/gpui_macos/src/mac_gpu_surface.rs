//! Theorem's native-only Metal → IOSurface producer for retained GPUI paint.
//!
//! This is a transport, not a creation/document store or a browser grant. The
//! source remains the compositor-owned external surface. Exactly three native
//! textures are retained; a consumer lease prevents overwrite and resize. No
//! IOSurface global ID, Mach port, JSON value, or guest-visible pointer is issued.
//! The privileged Servo adapter must separately bind admitted document/context
//! identity before consuming these frames. This module never performs readback.

use core_foundation::{
    base::{CFRelease, CFTypeRef, TCFType},
    dictionary::{CFDictionary, CFDictionaryRef},
    number::CFNumber,
    string::{CFString, CFStringRef},
};
use gpui::{
    ExternalGpuSurfaceError, ExternalGpuSurfaceFrameStamp, ExternalGpuSurfaceHandle,
    ExternalGpuSurfaceStatus,
};
use gpui_wgpu::wgpu;
use objc2::{
    encode::{Encoding, RefEncode},
    msg_send,
    rc::Retained,
    runtime::ProtocolObject,
};
use objc2_metal::{
    MTLClearColor, MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder, MTLCommandQueue,
    MTLLoadAction, MTLPixelFormat, MTLRenderPassDescriptor, MTLStorageMode, MTLStoreAction,
    MTLTexture, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage,
};
use std::{
    ffi::c_void,
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[repr(C)]
struct IOSurfaceOpaque {
    _private: [u8; 0],
}
// The Objective-C selector expects IOSurfaceRef, not a generic void pointer.
unsafe impl RefEncode for IOSurfaceOpaque {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("__IOSurface", &[]));
}

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceCreate(properties: CFDictionaryRef) -> *mut IOSurfaceOpaque;
    fn IOSurfaceAlignProperty(property: CFStringRef, value: usize) -> usize;
    fn IOSurfaceGetWidth(surface: *const IOSurfaceOpaque) -> usize;
    fn IOSurfaceGetHeight(surface: *const IOSurfaceOpaque) -> usize;
    fn IOSurfaceGetBytesPerRow(surface: *const IOSurfaceOpaque) -> usize;
    fn IOSurfaceGetPixelFormat(surface: *const IOSurfaceOpaque) -> u32;
    static kIOSurfaceWidth: CFStringRef;
    static kIOSurfaceHeight: CFStringRef;
    static kIOSurfaceBytesPerElement: CFStringRef;
    static kIOSurfaceBytesPerRow: CFStringRef;
    static kIOSurfaceAllocSize: CFStringRef;
    static kIOSurfacePixelFormat: CFStringRef;
}

const BGRA: u32 = u32::from_be_bytes(*b"BGRA");
const OUTPUT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
const SLOT_COUNT: usize = 3;

/// Native transport refusal. No alternate CPU transport is selected.
#[derive(Debug, thiserror::Error)]
pub enum MacGpuSurfaceError {
    #[error(transparent)]
    Source(#[from] ExternalGpuSurfaceError),
    #[error("the source is not an unorm premultiplied Metal color surface")]
    UnsupportedSource,
    #[error("the source belongs to a different renderer device or queue")]
    ForeignRenderer,
    #[error("all native transport resources are leased or awaiting GPU completion")]
    Busy,
    #[error("native IOSurface/Metal allocation or initialization failed: {0}")]
    Native(&'static str),
    #[error("native GPU polling failed")]
    Poll,
    #[error("native transport generation exhausted")]
    GenerationExhausted,
}

/// Owned, non-global IOSurface. Its pointer never crosses this native module's
/// boundary until an audited privileged adapter explicitly borrows it.
struct NativeSurface(NonNull<IOSurfaceOpaque>);
// IOSurface is a retained system storage object usable across native GPU threads.
// Its allocation and lifetime are fixed; production code never maps its bytes.
unsafe impl Send for NativeSurface {}
unsafe impl Sync for NativeSurface {}

impl Drop for NativeSurface {
    fn drop(&mut self) {
        // SAFETY: IOSurfaceCreate's +1 CF object is owned exactly once here.
        unsafe { CFRelease(self.0.as_ptr() as CFTypeRef) };
    }
}

struct Resource {
    view: wgpu::TextureView,
    texture: wgpu::Texture,
    surface: NativeSurface,
    size: (u32, u32),
    row_bytes: usize,
}

impl Resource {
    fn new(
        source: &ExternalGpuSurfaceHandle,
        size: (u32, u32),
    ) -> Result<Self, MacGpuSurfaceError> {
        let (width, height) = size;
        // SAFETY: System CFString constants are immortal. Width is validated by
        // the source's texture limits before this module can acquire a stamp.
        let row_bytes =
            unsafe { IOSurfaceAlignProperty(kIOSurfaceBytesPerRow, width as usize * 4) };
        let allocation = row_bytes
            .checked_mul(height as usize)
            .ok_or(MacGpuSurfaceError::Native("size overflow"))?;
        let properties = unsafe {
            CFDictionary::from_CFType_pairs(&[
                (
                    CFString::wrap_under_get_rule(kIOSurfaceWidth),
                    CFNumber::from(width as i64),
                ),
                (
                    CFString::wrap_under_get_rule(kIOSurfaceHeight),
                    CFNumber::from(height as i64),
                ),
                (
                    CFString::wrap_under_get_rule(kIOSurfaceBytesPerElement),
                    CFNumber::from(4_i64),
                ),
                (
                    CFString::wrap_under_get_rule(kIOSurfaceBytesPerRow),
                    CFNumber::from(row_bytes as i64),
                ),
                (
                    CFString::wrap_under_get_rule(kIOSurfaceAllocSize),
                    CFNumber::from(allocation as i64),
                ),
                (
                    CFString::wrap_under_get_rule(kIOSurfacePixelFormat),
                    CFNumber::from(BGRA as i64),
                ),
            ])
        };
        let surface = NativeSurface(
            NonNull::new(unsafe { IOSurfaceCreate(properties.as_concrete_TypeRef()) })
                .ok_or(MacGpuSurfaceError::Native("IOSurfaceCreate"))?,
        );
        // Refuse incompatible native storage; do not reinterpret its stride.
        if unsafe { IOSurfaceGetWidth(surface.0.as_ptr()) } != width as usize
            || unsafe { IOSurfaceGetHeight(surface.0.as_ptr()) } != height as usize
            || unsafe { IOSurfaceGetBytesPerRow(surface.0.as_ptr()) } != row_bytes
            || unsafe { IOSurfaceGetPixelFormat(surface.0.as_ptr()) } != BGRA
        {
            return Err(MacGpuSurfaceError::Native("IOSurface storage mismatch"));
        }
        // SAFETY: The Metal backend guard retains the exact GPUI device while
        // creating the native texture. No independent MTLDevice is constructed.
        let metal_device = unsafe { source.device().as_hal::<wgpu::hal::api::Metal>() }
            .ok_or(MacGpuSurfaceError::UnsupportedSource)?;
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::BGRA8Unorm,
                width as usize,
                height as usize,
                false,
            )
        };
        descriptor.setStorageMode(MTLStorageMode::Shared);
        descriptor.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
        // objc2-metal gates the typed IOSurface selector behind an additional
        // dependency; the actual selector here is identical, with a native CF
        // pointer whose lifetime is retained by Resource throughout texture use.
        let native: Option<Retained<ProtocolObject<dyn MTLTexture>>> = unsafe {
            msg_send![metal_device.raw_device(), newTextureWithDescriptor: &*descriptor, iosurface: surface.0.as_ptr(), plane: 0_usize]
        };
        let native = native.ok_or(MacGpuSurfaceError::Native("IOSurface Metal texture"))?;
        // wgpu's HAL import requires initialized contents. Clear on the same
        // native queue and wait once at allocation, with no mapped CPU pixels.
        let metal_queue = unsafe { source.queue().as_hal::<wgpu::hal::api::Metal>() }
            .ok_or(MacGpuSurfaceError::UnsupportedSource)?;
        let command = metal_queue
            .as_raw()
            .commandBuffer()
            .ok_or(MacGpuSurfaceError::Native("initialization command"))?;
        let pass = MTLRenderPassDescriptor::new();
        let color = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        color.setTexture(Some(&native));
        color.setLoadAction(MTLLoadAction::Clear);
        color.setStoreAction(MTLStoreAction::Store);
        color.setClearColor(MTLClearColor {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 0.0,
        });
        let encoder = command
            .renderCommandEncoderWithDescriptor(&pass)
            .ok_or(MacGpuSurfaceError::Native("initialization render pass"))?;
        encoder.endEncoding();
        command.commit();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while command.status() != MTLCommandBufferStatus::Completed
            && command.status() != MTLCommandBufferStatus::Error
        {
            if std::time::Instant::now() >= deadline {
                return Err(MacGpuSurfaceError::Native("initialization GPU timeout"));
            }
            std::thread::yield_now();
        }
        if command.status() != MTLCommandBufferStatus::Completed {
            return Err(MacGpuSurfaceError::Native("initialization GPU completion"));
        }
        drop(metal_queue);
        drop(metal_device);
        // SAFETY: This is the device's own initialized BGRA8 2D Metal texture,
        // single layer/mip/sample, with the descriptor's exact usage and size.
        let texture = unsafe {
            let raw = wgpu::hal::metal::Device::texture_from_raw(
                native,
                OUTPUT,
                MTLTextureType::Type2D,
                1,
                1,
                wgpu::hal::CopyExtent {
                    width,
                    height,
                    depth: 1,
                },
            );
            source
                .device()
                .create_texture_from_hal::<wgpu::hal::api::Metal>(
                    raw,
                    &wgpu::TextureDescriptor {
                        label: Some("gpui_native_iosurface"),
                        size: wgpu::Extent3d {
                            width,
                            height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: OUTPUT,
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                            | wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[],
                    },
                )
        };
        let view = texture.create_view(&Default::default());
        Ok(Self {
            surface,
            texture,
            view,
            size,
            row_bytes,
        })
    }
}

/// Completed native frame lease. Cloning retains its slot against reuse. The
/// stamp describes submitted GPUI paint; resource_generation describes the
/// transport allocation. Neither is an owner/document mutation receipt.
#[derive(Clone)]
pub struct MacGpuSurfaceFrame {
    resource: Arc<Resource>,
    stamp: ExternalGpuSurfaceFrameStamp,
    resource_generation: u64,
}
impl MacGpuSurfaceFrame {
    pub fn source_stamp(&self) -> ExternalGpuSurfaceFrameStamp {
        self.stamp
    }
    pub fn resource_generation(&self) -> u64 {
        self.resource_generation
    }
    pub fn size(&self) -> (u32, u32) {
        self.resource.size
    }
    pub fn row_bytes(&self) -> usize {
        self.resource.row_bytes
    }
    pub fn format(&self) -> wgpu::TextureFormat {
        OUTPUT
    }
    pub fn premultiplied_alpha(&self) -> bool {
        true
    }

    // For the forthcoming in-module privileged Servo adapter only. Never make
    // this a JS/MCP property or turn it into an IOSurface global lookup ID.
    #[allow(dead_code)]
    pub(crate) fn with_native_surface<R>(&self, borrow: impl FnOnce(*const c_void) -> R) -> R {
        borrow(self.resource.surface.0.as_ptr().cast())
    }
}

struct Pending {
    frame: MacGpuSurfaceFrame,
    completed: Arc<AtomicBool>,
}

/// Three-slot asynchronous producer on an existing GPUI compositor device.
/// Device polling is nonblocking. `take_completed` yields only GPU-complete,
/// currently generation-valid resources. Consumers must retain that lease until
/// their own GL fence completes before allowing overwrite.
pub struct MacGpuSurfaceProducer {
    source: ExternalGpuSurfaceHandle,
    slots: Vec<Arc<Resource>>,
    pending: Vec<Pending>,
    pipeline: wgpu::RenderPipeline,
    generation: u64,
    source_generation: u64,
    last_submitted: Option<ExternalGpuSurfaceFrameStamp>,
}

impl MacGpuSurfaceProducer {
    pub fn new(source: ExternalGpuSurfaceHandle) -> Result<Self, MacGpuSurfaceError> {
        require_source_ready(&source)?;
        if !matches!(
            source.format(),
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm
        ) || unsafe { source.device().as_hal::<wgpu::hal::api::Metal>() }.is_none()
        {
            return Err(MacGpuSurfaceError::UnsupportedSource);
        }
        let shader = source
            .device()
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("gpui_iosurface_gpu_copy"),
                source: wgpu::ShaderSource::Wgsl(
                    r#"
@group(0) @binding(0) var source: texture_2d<f32>;
@vertex fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let positions = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    return vec4(positions[index], 0.0, 1.0);
}
@fragment fn fragment(@builtin(position) pixel: vec4<f32>) -> @location(0) vec4<f32> {
    return textureLoad(source, vec2<i32>(pixel.xy), 0);
}
"#
                    .into(),
                ),
            });
        let pipeline = source
            .device()
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("gpui_iosurface_gpu_copy"),
                layout: None,
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vertex"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fragment"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: OUTPUT,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            });
        Ok(Self {
            source,
            slots: Vec::new(),
            pending: Vec::new(),
            pipeline,
            generation: 0,
            source_generation: 0,
            last_submitted: None,
        })
    }

    /// Enqueue the exact current published source once. `false` means the same
    /// submitted frame was already copied; no fake frame generation is minted.
    pub fn submit_current(&mut self) -> Result<bool, MacGpuSurfaceError> {
        self.poll()?;
        let stamp = self.source.published_frame_stamp()?;
        if !stamp.premultiplied_alpha() {
            return Err(MacGpuSurfaceError::UnsupportedSource);
        }
        if self.last_submitted == Some(stamp) {
            return Ok(false);
        }
        if self.source_generation != stamp.resource_generation() {
            // Old consumers and in-flight copies retain resources. Do not grow
            // the pool or overwrite a texture imported by the GL consumer.
            if !self.pending.is_empty()
                || self.slots.iter().any(|slot| Arc::strong_count(slot) != 1)
            {
                return Err(MacGpuSurfaceError::Busy);
            }
            let generation = self
                .generation
                .checked_add(1)
                .ok_or(MacGpuSurfaceError::GenerationExhausted)?;
            self.slots.clear();
            let slots = (0..SLOT_COUNT)
                .map(|_| Resource::new(&self.source, stamp.size()).map(Arc::new))
                .collect::<Result<Vec<_>, _>>()?;
            self.slots = slots;
            self.source_generation = stamp.resource_generation();
            self.generation = generation;
        }
        let resource = self
            .slots
            .iter()
            .find(|slot| Arc::strong_count(slot) == 1)
            .cloned()
            .ok_or(MacGpuSurfaceError::Busy)?;
        let reader = self.source.acquire_published_frame(stamp)?;
        let bind = self
            .source
            .device()
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.pipeline.get_bind_group_layout(0),
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(reader.view()),
                }],
            });
        let mut encoder = self
            .source
            .device()
            .create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &resource.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.draw(0..3, 0..1);
        }
        reader.submit_copy([encoder.finish()])?;
        let completed = Arc::new(AtomicBool::new(false));
        let complete = Arc::clone(&completed);
        self.source.queue().on_submitted_work_done(move || {
            complete.store(true, Ordering::Release);
        });
        self.pending.push(Pending {
            frame: MacGpuSurfaceFrame {
                resource,
                stamp,
                resource_generation: self.generation,
            },
            completed,
        });
        self.last_submitted = Some(stamp);
        Ok(true)
    }

    fn poll(&self) -> Result<(), MacGpuSurfaceError> {
        require_source_ready(&self.source)?;
        self.source
            .device()
            .poll(wgpu::PollType::Poll)
            .map_err(|_| MacGpuSurfaceError::Poll)?;
        Ok(())
    }

    /// Take the oldest completed copy in submission order. A pending resize,
    /// closed/lost source, or replaced source generation invalidates undelivered
    /// resources. Already delivered leases remain owned until consumer release.
    pub fn take_completed(&mut self) -> Result<Option<MacGpuSurfaceFrame>, MacGpuSurfaceError> {
        self.poll()?;
        let current = match self.source.published_frame_stamp() {
            Ok(stamp) => stamp,
            Err(error) => {
                self.pending
                    .retain(|pending| !pending.completed.load(Ordering::Acquire));
                return Err(error.into());
            }
        };
        if self
            .pending
            .first()
            .is_none_or(|pending| !pending.completed.load(Ordering::Acquire))
        {
            return Ok(None);
        }
        let pending = self.pending.remove(0);
        if pending.frame.stamp.resource_generation() != current.resource_generation() {
            return Err(ExternalGpuSurfaceError::StaleFrame.into());
        }
        Ok(Some(pending.frame))
    }

    pub fn allocated_resource_count(&self) -> usize {
        self.slots.len()
    }
    pub fn pending_frame_count(&self) -> usize {
        self.pending.len()
    }
}

fn require_source_ready(source: &ExternalGpuSurfaceHandle) -> Result<(), MacGpuSurfaceError> {
    match source.status() {
        ExternalGpuSurfaceStatus::Ready => Ok(()),
        ExternalGpuSurfaceStatus::DeviceLost => Err(ExternalGpuSurfaceError::DeviceLost.into()),
        ExternalGpuSurfaceStatus::Closed => Err(ExternalGpuSurfaceError::Closed.into()),
    }
}

/// Versioned same-process native ABI. This conveys storage lifetime, never
/// browser authorization. The host binds its own admitted document/context and
/// presentation epoch before passing the lease to Servo. No Rust layout crosses
/// the dylib boundary and no global IOSurface identifier is allocated.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct NativeGpuFrameMetadataV1 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub width: u32,
    pub height: u32,
    pub bytes_per_row: u64,
    pub source_registry: u64,
    pub source_surface: u64,
    pub source_resource_generation: u64,
    pub source_frame_generation: u64,
    pub transport_resource_generation: u64,
    /// FourCC 'BGRA', unorm8, top row first, premultiplied alpha.
    pub pixel_format: u32,
    pub premultiplied_alpha: u32,
}

/// One owned native reference. Not Copy/Clone: duplicate using retain, then
/// release exactly once for each reference after all consumer GPU access ends.
/// Function pointers refer to the producing dylib, which must remain loaded.
#[repr(C)]
pub struct NativeGpuFrameLeaseV1 {
    pub metadata: NativeGpuFrameMetadataV1,
    pub opaque: *const c_void,
    pub retain: unsafe extern "C" fn(*const c_void),
    pub release: unsafe extern "C" fn(*const c_void),
    pub borrow_iosurface: unsafe extern "C" fn(*const c_void) -> *const c_void,
}

impl MacGpuSurfaceFrame {
    /// Transfer one owned GPU-complete lease to the privileged same-process host.
    /// The caller must release it; copying the C struct does not retain it.
    pub fn into_native_lease(self) -> NativeGpuFrameLeaseV1 {
        let (source_registry, source_surface) = self.stamp.surface().native_identity();
        let metadata = NativeGpuFrameMetadataV1 {
            abi_version: 1,
            struct_size: std::mem::size_of::<NativeGpuFrameMetadataV1>() as u32,
            width: self.size().0,
            height: self.size().1,
            bytes_per_row: self.row_bytes() as u64,
            source_registry,
            source_surface,
            source_resource_generation: self.stamp.resource_generation(),
            source_frame_generation: self.stamp.frame_generation(),
            transport_resource_generation: self.resource_generation,
            pixel_format: BGRA,
            premultiplied_alpha: 1,
        };
        NativeGpuFrameLeaseV1 {
            metadata,
            opaque: Arc::into_raw(Arc::new(self)).cast(),
            retain: native_retain,
            release: native_release,
            borrow_iosurface: native_borrow_iosurface,
        }
    }
}

// SAFETY contracts: only a live opaque value returned by into_native_lease may
// enter these functions. Retain/release must be balanced, no call may race the
// final release, and the native consumer must finish GPU reads before release.
unsafe extern "C" fn native_retain(opaque: *const c_void) {
    unsafe { Arc::increment_strong_count(opaque.cast::<MacGpuSurfaceFrame>()) };
}
unsafe extern "C" fn native_release(opaque: *const c_void) {
    drop(unsafe { Arc::from_raw(opaque.cast::<MacGpuSurfaceFrame>()) });
}
unsafe extern "C" fn native_borrow_iosurface(opaque: *const c_void) -> *const c_void {
    unsafe { &*opaque.cast::<MacGpuSurfaceFrame>() }
        .resource
        .surface
        .0
        .as_ptr()
        .cast()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::AtomicBool,
        time::{Duration, Instant},
    };

    // Diagnostic-only mapping. Production transport contains no IOSurfaceLock,
    // base address access, MAP_READ buffer, or copy-to-buffer operation.
    #[link(name = "IOSurface", kind = "framework")]
    unsafe extern "C" {
        fn IOSurfaceLock(surface: *const IOSurfaceOpaque, options: u32, seed: *mut u32) -> i32;
        fn IOSurfaceUnlock(surface: *const IOSurfaceOpaque, options: u32, seed: *mut u32) -> i32;
        fn IOSurfaceGetBaseAddress(surface: *const IOSurfaceOpaque) -> *const u8;
    }

    fn host() -> anyhow::Result<(ExternalGpuSurfaceHandle, Arc<AtomicBool>)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            flags: wgpu::InstanceFlags::default(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display: None,
        });
        let adapter = gpui::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))?;
        assert_eq!(adapter.get_info().backend, wgpu::Backend::Metal);
        println!("IOSurface producer adapter: {:?}", adapter.get_info());
        let (device, queue) = gpui::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor {
                required_limits: wgpu::Limits::downlevel_defaults()
                    .using_resolution(adapter.limits())
                    .using_alignment(adapter.limits()),
                ..Default::default()
            }),
        )?;
        let lost = Arc::new(AtomicBool::new(false));
        let source = ExternalGpuSurfaceHandle::new(
            Arc::new(gpui::ExternalGpuSurfaceRegistry::new()),
            Arc::new(device),
            Arc::new(queue),
            8,
            4,
            wgpu::TextureFormat::Bgra8Unorm,
            Arc::clone(&lost),
            Arc::new(|| {}),
        )?;
        Ok((source, lost))
    }

    fn paint(source: &ExternalGpuSurfaceHandle, color: wgpu::Color) -> anyhow::Result<()> {
        let frame = source.acquire_frame()?;
        let mut encoder = source.device().create_command_encoder(&Default::default());
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: frame.view(),
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
        }
        frame.submit_premultiplied_and_present([encoder.finish()])?;
        Ok(())
    }

    fn complete(producer: &mut MacGpuSurfaceProducer) -> anyhow::Result<MacGpuSurfaceFrame> {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(frame) = producer.take_completed()? {
                return Ok(frame);
            }
            anyhow::ensure!(Instant::now() < until, "GPU copy did not complete");
            std::thread::yield_now();
        }
    }

    fn assert_pixels(frame: &MacGpuSurfaceFrame, expected: [u8; 4]) {
        let surface = frame.resource.surface.0.as_ptr();
        unsafe {
            assert_eq!(IOSurfaceLock(surface, 1, std::ptr::null_mut()), 0);
            let bytes = IOSurfaceGetBaseAddress(surface);
            assert!(!bytes.is_null());
            for y in 0..frame.size().1 as usize {
                for x in 0..frame.size().0 as usize {
                    let pixel =
                        std::slice::from_raw_parts(bytes.add(y * frame.row_bytes() + x * 4), 4);
                    assert_eq!(pixel, expected, "IOSurface BGRA pixel {x},{y}");
                }
            }
            assert_eq!(IOSurfaceUnlock(surface, 1, std::ptr::null_mut()), 0);
        }
    }

    #[test]
    #[ignore = "requires actual native Metal and IOSurface"]
    fn iosurface_gpu_copy_preserves_independent_quadrant_orientation() -> anyhow::Result<()> {
        let (source, _) = host()?;
        let shader = source
            .device()
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("independent_gpu_quadrant_source"),
                source: wgpu::ShaderSource::Wgsl(
                    r#"
@vertex fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let positions = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    return vec4(positions[index], 0.0, 1.0);
}
@fragment fn fragment(@builtin(position) pixel: vec4<f32>) -> @location(0) vec4<f32> {
    if pixel.y < 2.0 {
        if pixel.x < 4.0 { return vec4(1.0, 0.0, 0.0, 1.0); }
        return vec4(0.0, 1.0, 0.0, 1.0);
    }
    if pixel.x < 4.0 { return vec4(0.0, 0.0, 1.0, 1.0); }
    return vec4(0.25, 0.0, 0.0, 0.5);
}
"#
                    .into(),
                ),
            });
        let pipeline = source
            .device()
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: None,
                layout: None,
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vertex"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fragment"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: source.format(),
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            });
        let paint = source.acquire_frame()?;
        let mut encoder = source.device().create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: paint.view(),
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&pipeline);
            pass.draw(0..3, 0..1);
        }
        paint.submit_premultiplied_and_present([encoder.finish()])?;
        let mut producer = MacGpuSurfaceProducer::new(source)?;
        producer.submit_current()?;
        let frame = complete(&mut producer)?;
        let surface = frame.resource.surface.0.as_ptr();
        unsafe {
            assert_eq!(IOSurfaceLock(surface, 1, std::ptr::null_mut()), 0);
            let base = IOSurfaceGetBaseAddress(surface);
            assert!(!base.is_null());
            // Fixed independent expected regions reject vertical/horizontal
            // inversion, RGBA/BGRA confusion, and alpha multiplication twice.
            for y in 0..4 {
                for x in 0..8 {
                    let expected = match (y < 2, x < 4) {
                        (true, true) => [0, 0, 255, 255],
                        (true, false) => [0, 255, 0, 255],
                        (false, true) => [255, 0, 0, 255],
                        (false, false) => [0, 0, 64, 128],
                    };
                    assert_eq!(
                        std::slice::from_raw_parts(base.add(y * frame.row_bytes() + x * 4), 4),
                        expected,
                        "independent quadrant pixel {x},{y}"
                    );
                }
            }
            assert_eq!(IOSurfaceUnlock(surface, 1, std::ptr::null_mut()), 0);
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires actual native Metal and IOSurface"]
    fn iosurface_gpu_copy_changes_pixels_and_preserves_premultiplied_alpha() -> anyhow::Result<()> {
        let (source, _) = host()?;
        let mut producer = MacGpuSurfaceProducer::new(source.clone())?;
        assert!(matches!(
            producer.submit_current(),
            Err(MacGpuSurfaceError::Source(
                ExternalGpuSurfaceError::NotPublished
            ))
        ));
        for (index, (color, expected)) in [
            (wgpu::Color::RED, [0, 0, 255, 255]),
            (wgpu::Color::GREEN, [0, 255, 0, 255]),
            (wgpu::Color::BLUE, [255, 0, 0, 255]),
            (
                wgpu::Color {
                    r: 0.25,
                    g: 0.0,
                    b: 0.0,
                    a: 0.5,
                },
                [0, 0, 64, 128],
            ),
        ]
        .into_iter()
        .cycle()
        .take(16)
        .enumerate()
        {
            paint(&source, color)?;
            assert!(producer.submit_current()?);
            assert!(
                !producer.submit_current()?,
                "same source stamp must not fabricate a fresh frame"
            );
            let frame = complete(&mut producer)?;
            assert_eq!(frame.source_stamp().frame_generation(), index as u64 + 1);
            assert_eq!(frame.resource_generation(), 1);
            assert_eq!(frame.format(), wgpu::TextureFormat::Bgra8Unorm);
            assert!(frame.premultiplied_alpha());
            assert!(
                !frame
                    .resource
                    .texture
                    .usage()
                    .contains(wgpu::TextureUsages::COPY_SRC)
            );
            assert_eq!(producer.allocated_resource_count(), 3);
            assert_eq!(producer.pending_frame_count(), 0);
            assert_pixels(&frame, expected);
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires actual native Metal and IOSurface"]
    fn iosurface_pool_refuses_held_slots_resize_and_lost_source() -> anyhow::Result<()> {
        let (source, lost) = host()?;
        let mut producer = MacGpuSurfaceProducer::new(source.clone())?;
        let mut held = Vec::new();
        for color in [wgpu::Color::RED, wgpu::Color::GREEN, wgpu::Color::BLUE] {
            paint(&source, color)?;
            producer.submit_current()?;
            held.push(complete(&mut producer)?);
        }
        paint(&source, wgpu::Color::WHITE)?;
        assert!(matches!(
            producer.submit_current(),
            Err(MacGpuSurfaceError::Busy)
        ));
        assert_eq!(producer.allocated_resource_count(), 3);
        assert_pixels(&held[0], [0, 0, 255, 255]);
        assert_pixels(&held[1], [0, 255, 0, 255]);
        assert_pixels(&held[2], [255, 0, 0, 255]);
        drop(held.remove(0));
        assert!(producer.submit_current()?);
        held.push(complete(&mut producer)?);
        assert_pixels(held.last().unwrap(), [255, 255, 255, 255]);
        source.request_resize(4, 8);
        assert!(matches!(
            producer.submit_current(),
            Err(MacGpuSurfaceError::Source(
                ExternalGpuSurfaceError::ResizePending
            ))
        ));
        paint(&source, wgpu::Color::RED)?;
        assert!(
            matches!(producer.submit_current(), Err(MacGpuSurfaceError::Busy)),
            "resize must not allocate another pool while consumers hold old resources"
        );
        held.clear();
        assert!(producer.submit_current()?);
        let frame = complete(&mut producer)?;
        assert_eq!(frame.size(), (4, 8));
        assert_eq!(frame.resource_generation(), 2);
        assert_eq!(producer.allocated_resource_count(), 3);
        assert_pixels(&frame, [0, 0, 255, 255]);
        lost.store(true, Ordering::Release);
        assert!(matches!(
            MacGpuSurfaceProducer::new(source.clone()),
            Err(MacGpuSurfaceError::Source(
                ExternalGpuSurfaceError::DeviceLost
            ))
        ));
        assert!(matches!(
            producer.submit_current(),
            Err(MacGpuSurfaceError::Source(
                ExternalGpuSurfaceError::DeviceLost
            ))
        ));
        assert!(matches!(
            producer.take_completed(),
            Err(MacGpuSurfaceError::Source(
                ExternalGpuSurfaceError::DeviceLost
            ))
        ));
        Ok(())
    }

    #[test]
    #[ignore = "requires actual native Metal and IOSurface"]
    fn iosurface_undelivered_copy_is_refused_after_source_resize() -> anyhow::Result<()> {
        let (source, _) = host()?;
        let mut producer = MacGpuSurfaceProducer::new(source.clone())?;
        paint(&source, wgpu::Color::BLUE)?;
        producer.submit_current()?;
        source.device().poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(Duration::from_secs(5)),
        })?;
        source.request_resize(4, 8);
        assert!(matches!(
            producer.take_completed(),
            Err(MacGpuSurfaceError::Source(
                ExternalGpuSurfaceError::ResizePending
            ))
        ));
        assert_eq!(
            producer.pending_frame_count(),
            0,
            "completed pre-resize copies must be discarded"
        );
        paint(&source, wgpu::Color::RED)?;
        producer.submit_current()?;
        let frame = complete(&mut producer)?;
        assert_eq!(frame.size(), (4, 8));
        assert_eq!(frame.resource_generation(), 2);
        assert_pixels(&frame, [0, 0, 255, 255]);
        drop(frame);
        paint(&source, wgpu::Color::BLUE)?;
        producer.submit_current()?;
        source.request_resize(8, 4);
        paint(&source, wgpu::Color::GREEN)?;
        source.device().poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(Duration::from_secs(5)),
        })?;
        assert!(matches!(
            producer.take_completed(),
            Err(MacGpuSurfaceError::Source(
                ExternalGpuSurfaceError::StaleFrame
            ))
        ));
        assert_eq!(producer.pending_frame_count(), 0);
        producer.submit_current()?;
        let frame = complete(&mut producer)?;
        assert_eq!(frame.size(), (8, 4));
        assert_eq!(frame.resource_generation(), 3);
        assert_pixels(&frame, [0, 255, 0, 255]);
        Ok(())
    }

    #[test]
    #[ignore = "requires actual native Metal and IOSurface"]
    fn native_frame_abi_retains_storage_across_threads_until_final_release() -> anyhow::Result<()> {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<MacGpuSurfaceFrame>();
        let (source, _) = host()?;
        let mut producer = MacGpuSurfaceProducer::new(source.clone())?;
        paint(&source, wgpu::Color::BLUE)?;
        producer.submit_current()?;
        let frame = complete(&mut producer)?;
        let storage = Arc::downgrade(&frame.resource);
        let lease = frame.into_native_lease();
        assert_eq!(
            lease.metadata.struct_size as usize,
            std::mem::size_of::<NativeGpuFrameMetadataV1>()
        );
        assert_eq!(std::mem::size_of::<NativeGpuFrameMetadataV1>(), 72);
        assert_eq!(std::mem::size_of::<NativeGpuFrameLeaseV1>(), 104);
        assert_eq!(lease.metadata.abi_version, 1);
        assert_eq!(lease.metadata.pixel_format, u32::from_be_bytes(*b"BGRA"));
        assert_eq!(lease.metadata.premultiplied_alpha, 1);
        assert_eq!(lease.metadata.source_frame_generation, 1);
        assert_eq!(
            (
                lease.metadata.source_registry,
                lease.metadata.source_surface
            ),
            source.id().native_identity()
        );
        let opaque = lease.opaque as usize;
        let retain = lease.retain;
        let release = lease.release;
        let borrow = lease.borrow_iosurface;
        unsafe {
            retain(lease.opaque);
            release(lease.opaque);
        }
        drop(producer);
        assert!(
            storage.upgrade().is_some(),
            "retained consumer storage must outlive producer"
        );
        std::thread::spawn(move || unsafe {
            let pointer = opaque as *const c_void;
            let surface = borrow(pointer).cast::<IOSurfaceOpaque>();
            assert!(!surface.is_null());
            assert_eq!(IOSurfaceGetWidth(surface), 8);
            assert_eq!(IOSurfaceGetHeight(surface), 4);
            assert_eq!(
                IOSurfaceGetPixelFormat(surface),
                u32::from_be_bytes(*b"BGRA")
            );
            release(pointer);
        })
        .join()
        .unwrap();
        assert!(
            storage.upgrade().is_none(),
            "final native release must free consumer storage"
        );
        Ok(())
    }
}
