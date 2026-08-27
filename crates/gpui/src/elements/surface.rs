use crate::{
    App, Bounds, Element, ElementId, ExternalGpuSurfaceHandle, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, ObjectFit, Pixels, Style, StyleRefinement, Styled, TransformationMatrix,
    Window,
};
#[cfg(target_os = "macos")]
use core_video::pixel_buffer::CVPixelBuffer;
use refineable::Refineable;

/// A source of a surface's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceSource {
    /// A macOS image buffer from CoreVideo
    #[cfg(target_os = "macos")]
    Surface(CVPixelBuffer),
    /// A texture surface rendered by an external producer on GPUI's device.
    ExternalGpu(ExternalGpuSurfaceHandle),
}

#[cfg(target_os = "macos")]
impl From<CVPixelBuffer> for SurfaceSource {
    fn from(value: CVPixelBuffer) -> Self {
        SurfaceSource::Surface(value)
    }
}

/// A surface element.
pub struct Surface {
    source: SurfaceSource,
    object_fit: ObjectFit,
    transformation: TransformationMatrix,
    style: StyleRefinement,
}

/// Create a new surface element.
#[cfg(target_os = "macos")]
pub fn surface(source: impl Into<SurfaceSource>) -> Surface {
    Surface {
        source: source.into(),
        object_fit: ObjectFit::Contain,
        transformation: TransformationMatrix::unit(),
        style: Default::default(),
    }
}

/// Create an element that composites an external GPU surface inside GPUI.
pub fn external_gpu_surface(source: ExternalGpuSurfaceHandle) -> Surface {
    Surface {
        source: SurfaceSource::ExternalGpu(source),
        object_fit: ObjectFit::Fill,
        transformation: TransformationMatrix::unit(),
        style: Default::default(),
    }
}

impl Surface {
    /// Set the object fit for the image.
    pub fn object_fit(mut self, object_fit: ObjectFit) -> Self {
        self.object_fit = object_fit;
        self
    }

    /// Apply a scene-space transform while retaining the GPUI content mask.
    pub fn with_transformation(mut self, transformation: TransformationMatrix) -> Self {
        self.transformation = transformation;
        self
    }
}

impl Element for Surface {
    type RequestLayoutState = Style;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.refine(&self.style);
        let layout_id = window.request_layout(style.clone(), [], cx);
        (layout_id, style)
    }

    fn prepaint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let surface = match &self.source {
            SurfaceSource::ExternalGpu(surface) => Some(surface),
            #[cfg(target_os = "macos")]
            SurfaceSource::Surface(_) => None,
        };
        if let Some(surface) = surface {
            let scale_factor = window.scale_factor();
            let width = (bounds.size.width.0 * scale_factor).round().max(1.0) as u32;
            let height = (bounds.size.height.0 * scale_factor).round().max(1.0) as u32;
            if surface.size().ok() != Some((width, height)) {
                surface.request_resize(width, height);
            }
        }
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        #[cfg_attr(not(target_os = "macos"), allow(unused_variables))] bounds: Bounds<Pixels>,
        style: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        #[cfg_attr(not(target_os = "macos"), allow(unused_variables))] window: &mut Window,
        cx: &mut App,
    ) {
        style.paint(bounds, window, cx, |window, _cx| {
            match &self.source {
                #[cfg(target_os = "macos")]
                SurfaceSource::Surface(surface) => {
                    let size = crate::size(surface.get_width().into(), surface.get_height().into());
                    let new_bounds = self.object_fit.get_bounds(bounds, size);
                    // TODO: Add support for corner_radii
                    window.paint_surface(new_bounds, surface.clone());
                }
                SurfaceSource::ExternalGpu(surface) => {
                    window.paint_external_gpu_surface(bounds, surface, self.transformation);
                }
            }
        });
    }
}

impl IntoElement for Surface {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Styled for Surface {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}
