//! Paint a retained element subtree into an existing compositor-owned GPU surface.
//!
//! Theorem adaptation of gpui-fusion's `UiTexture3d` sizing contract at
//! revision 2f0e58241e4d. Capture changes the presentation target, not the
//! entity, document, or session that owns the element's content.

use crate::{
    AnyElement, App, Bounds, DevicePixels, Element, ElementId, ExternalGpuSurfaceHandle,
    GlobalElementId, InspectorElementId, IntoElement, LayoutId, Pixels, Point, Size, Style, Window,
    size,
};

/// A refused capture publication, available to the trusted host after presentation.
#[derive(Clone, Debug)]
pub struct RetainedCaptureFailure {
    /// The compositor lease whose current frame was not published.
    pub target: crate::ExternalGpuSurfaceId,
    /// The refusal reason; the host may recreate the lease or restore focused UI.
    pub reason: String,
}

/// Logical layout and bounded raster density for one retained subtree capture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetainedCaptureConfig {
    logical_size: Size<Pixels>,
    scale_factor: f32,
    max_dimension: u32,
}

impl RetainedCaptureConfig {
    /// Validate dimensions and reduce density uniformly to fit the device cap.
    ///
    /// The allocation is bounded by both the caller's device limit and 2048
    /// pixels per dimension. Logical layout dimensions are never reduced.
    pub fn new(
        logical_size: Size<Pixels>,
        requested_density: f32,
        max_dimension: u32,
    ) -> anyhow::Result<Self> {
        let width = f32::from(logical_size.width);
        let height = f32::from(logical_size.height);
        anyhow::ensure!(
            width.is_finite() && width > 0. && height.is_finite() && height > 0.,
            "retained capture logical dimensions must be finite and positive"
        );
        anyhow::ensure!(
            requested_density.is_finite() && requested_density > 0.,
            "retained capture density must be finite and positive"
        );
        anyhow::ensure!(max_dimension > 0, "retained capture device limit is zero");
        let max_dimension = max_dimension.min(2048);
        let scale_factor = requested_density.min(max_dimension as f32 / width.max(height));
        anyhow::ensure!(
            scale_factor.is_finite() && scale_factor > 0.,
            "retained capture effective density is not representable"
        );
        Ok(Self {
            logical_size,
            scale_factor,
            max_dimension,
        })
    }

    /// Layout dimensions, independent of the destination mesh or window.
    pub fn logical_size(self) -> Size<Pixels> {
        self.logical_size
    }

    /// Effective device pixels per logical pixel after applying the cap.
    pub fn scale_factor(self) -> f32 {
        self.scale_factor
    }

    /// Pixel dimensions rounded up to cover the complete logical layout.
    pub fn pixel_size(self) -> Size<DevicePixels> {
        let extent = |logical: Pixels| {
            DevicePixels(
                (f32::from(logical) * self.scale_factor)
                    .ceil()
                    .clamp(1., self.max_dimension as f32) as i32,
            )
        };
        size(
            extent(self.logical_size.width),
            extent(self.logical_size.height),
        )
    }
}

/// Capture the supplied element without constructing a replacement view or session.
///
/// This element reserves its logical size but paints only to `target`. Input
/// ownership is governed by the window capture scope; this wrapper introduces
/// no pointer or keyboard handlers. The target is validated when it is rendered,
/// so resizing and device loss remain the compositor's responsibility.
pub fn retained_capture(
    id: impl Into<ElementId>,
    config: RetainedCaptureConfig,
    target: ExternalGpuSurfaceHandle,
    content: impl IntoElement,
) -> RetainedCaptureElement {
    RetainedCaptureElement {
        id: id.into(),
        config,
        target,
        content: content.into_any_element(),
    }
}

/// One retained child whose paint commands are redirected to a GPU surface.
pub struct RetainedCaptureElement {
    id: ElementId,
    config: RetainedCaptureConfig,
    target: ExternalGpuSurfaceHandle,
    content: AnyElement,
}

impl IntoElement for RetainedCaptureElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RetainedCaptureElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let style = Style {
            size: size(
                self.config.logical_size().width.into(),
                self.config.logical_size().height.into(),
            ),
            ..Style::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_retained_capture(self.config, |window| {
            self.content.prepaint_as_root(
                Point::default(),
                self.config.logical_size().into(),
                window,
                cx,
            );
        });
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        window.paint_retained_capture(self.config, self.target.clone(), |window| {
            self.content.paint(window, cx);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::RetainedCaptureConfig;
    use crate::{DevicePixels, px, size};

    #[test]
    fn rejects_nonfinite_nonpositive_dimensions_and_density() {
        for invalid in [0., -1., f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(RetainedCaptureConfig::new(size(px(invalid), px(100.)), 1., 4096).is_err());
            assert!(RetainedCaptureConfig::new(size(px(100.), px(invalid)), 1., 4096).is_err());
            assert!(RetainedCaptureConfig::new(size(px(100.), px(100.)), invalid, 4096).is_err());
        }
        assert!(RetainedCaptureConfig::new(size(px(100.), px(100.)), 1., 0).is_err());
    }

    #[test]
    fn caps_density_uniformly_without_changing_logical_layout() {
        let logical = size(px(1600.), px(800.));
        let config = RetainedCaptureConfig::new(logical, 4., 4096).unwrap();
        assert_eq!(config.logical_size(), logical);
        assert_eq!(config.scale_factor(), 1.28);
        assert_eq!(
            config.pixel_size(),
            size(DevicePixels(2048), DevicePixels(1024))
        );
        let device_limited = RetainedCaptureConfig::new(logical, 4., 512).unwrap();
        assert_eq!(device_limited.scale_factor(), 0.32);
        assert_eq!(
            device_limited.pixel_size(),
            size(DevicePixels(512), DevicePixels(256))
        );
    }

    #[test]
    fn rounds_fractional_coverage_up_and_allocates_at_least_one_pixel() {
        let config = RetainedCaptureConfig::new(size(px(100.25), px(50.75)), 1.5, 2048).unwrap();
        assert_eq!(
            config.pixel_size(),
            size(DevicePixels(151), DevicePixels(77))
        );
        let tiny = RetainedCaptureConfig::new(size(px(0.01), px(0.02)), 0.1, 2048).unwrap();
        assert_eq!(tiny.pixel_size(), size(DevicePixels(1), DevicePixels(1)));
    }

    #[test]
    fn finite_extremes_do_not_overflow_allocation() {
        let config =
            RetainedCaptureConfig::new(size(px(f32::MAX), px(1.)), f32::MAX, 4096).unwrap();
        assert_eq!(
            config.pixel_size(),
            size(DevicePixels(2048), DevicePixels(1))
        );
    }
}
