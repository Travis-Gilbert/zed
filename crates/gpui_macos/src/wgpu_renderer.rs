use foreign_types::ForeignType;
use gpui::{
    DevicePixels, ExternalGpuSurfaceError, ExternalGpuSurfaceHandle, ExternalGpuSurfaceInvalidator,
    GpuSpecs, Scene, Size,
};
use gpui_wgpu::{GpuContext, WgpuAtlas, WgpuRenderer, WgpuSurfaceConfig};
#[cfg(any(test, feature = "test-support"))]
use image::RgbaImage;
use raw_window_handle::{
    AppKitWindowHandle, DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle,
    RawWindowHandle, WindowHandle,
};
use std::{ffi::c_void, fmt, ptr::NonNull, rc::Rc, sync::Arc};

pub type Context = GpuContext;

#[derive(Clone, Copy)]
struct MacSurfaceTarget {
    native_view: NonNull<c_void>,
}

impl fmt::Debug for MacSurfaceTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MacSurfaceTarget")
            .field("native_view", &self.native_view)
            .finish()
    }
}

unsafe impl Send for MacSurfaceTarget {}
unsafe impl Sync for MacSurfaceTarget {}

impl HasWindowHandle for MacSurfaceTarget {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let handle = AppKitWindowHandle::new(self.native_view);
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::AppKit(handle)) })
    }
}

impl HasDisplayHandle for MacSurfaceTarget {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::appkit())
    }
}

pub struct Renderer {
    context: Context,
    surface_target: MacSurfaceTarget,
    layer: metal::MetalLayer,
    inner: Option<WgpuRenderer>,
    transparent: bool,
}

pub unsafe fn new_renderer(
    context: Context,
    _native_window: *mut c_void,
    native_view: *mut c_void,
    _bounds: gpui::Size<f32>,
    transparent: bool,
) -> Renderer {
    // SAFETY: MacWindow creates the NSView immediately before constructing the renderer
    // and checks it for null before passing it here.
    let native_view = unsafe { NonNull::new_unchecked(native_view) };
    let layer = metal::MetalLayer::new();
    layer.set_pixel_format(metal::MTLPixelFormat::BGRA8Unorm);
    layer.set_opaque(!transparent);
    layer.set_maximum_drawable_count(3);

    Renderer {
        context,
        surface_target: MacSurfaceTarget { native_view },
        layer,
        inner: None,
        transparent,
    }
}

impl Renderer {
    pub fn initialize(&mut self, size: Size<DevicePixels>) -> anyhow::Result<()> {
        if self.inner.is_some() {
            return Ok(());
        }
        self.inner = Some(WgpuRenderer::new(
            Rc::clone(&self.context),
            &self.surface_target,
            WgpuSurfaceConfig {
                size,
                transparent: self.transparent,
                preferred_present_mode: None,
            },
            None,
        )?);
        Ok(())
    }

    fn inner(&self) -> &WgpuRenderer {
        self.inner
            .as_ref()
            .expect("macOS WGPU renderer must be initialized after attaching its CAMetalLayer")
    }

    fn inner_mut(&mut self) -> &mut WgpuRenderer {
        self.inner
            .as_mut()
            .expect("macOS WGPU renderer must be initialized after attaching its CAMetalLayer")
    }

    pub fn layer(&self) -> Option<&metal::MetalLayerRef> {
        Some(self.layer.as_ref())
    }

    pub fn layer_ptr(&self) -> *mut c_void {
        self.layer.as_ptr().cast()
    }

    pub fn set_presents_with_transaction(&mut self, enabled: bool) {
        self.layer.set_presents_with_transaction(enabled);
    }

    pub fn update_drawable_size(&mut self, size: Size<DevicePixels>) {
        if let Some(inner) = self.inner.as_mut() {
            inner.update_drawable_size(size);
        }
    }

    pub fn update_transparency(&mut self, transparent: bool) {
        self.transparent = transparent;
        if let Some(inner) = self.inner.as_mut() {
            inner.update_transparency(transparent);
        }
    }

    pub fn draw(&mut self, scene: &Scene) -> bool {
        self.inner_mut().draw(scene)
    }

    pub fn destroy(&mut self) {
        if let Some(inner) = self.inner.as_mut() {
            inner.destroy();
        }
        self.inner = None;
    }

    pub fn sprite_atlas(&self) -> &Arc<WgpuAtlas> {
        self.inner().sprite_atlas()
    }

    pub fn supports_dual_source_blending(&self) -> bool {
        self.inner().supports_dual_source_blending()
    }

    pub fn gpu_specs(&self) -> GpuSpecs {
        self.inner().gpu_specs()
    }

    pub fn create_external_gpu_surface(
        &self,
        width: u32,
        height: u32,
        format: gpui::wgpu::TextureFormat,
        invalidator: ExternalGpuSurfaceInvalidator,
    ) -> Result<ExternalGpuSurfaceHandle, ExternalGpuSurfaceError> {
        self.inner()
            .create_external_gpu_surface(width, height, format, invalidator)
    }

    pub fn device_lost(&self) -> bool {
        self.inner().device_lost()
    }

    pub fn recover(&mut self) -> anyhow::Result<()> {
        let surface_target = self.surface_target;
        self.inner_mut().recover(&surface_target)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn render_to_image(&mut self, _scene: &Scene) -> anyhow::Result<RgbaImage> {
        anyhow::bail!("render_to_image is not implemented by the macOS WGPU compositor")
    }
}
