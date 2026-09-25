//! Browser-native realization of a GPUI leaf.
//!
//! A GPUI leaf is ordinarily painted: its contents become vertices and
//! textures in the scene, and every behavior a person expects of the control
//! is GPUI's to reproduce. For an editable leaf that reproduction is where the
//! browser costs the most, because a browser does not offer caret, selection,
//! IME, autocorrect, spellcheck or the virtual keyboard as primitives. It
//! offers them only as properties of real elements.
//!
//! The HTML-in-Canvas proposal closes that gap, though not by the route its
//! prose suggests. What a browser ships is a canvas that lays its descendants
//! out (`layoutsubtree`) and can draw one of them into itself
//! (`drawElementImage`, inside a `paint` event). Input, focus, IME and
//! accessibility keep following the element's ordinary CSS box; no call moves
//! them to where the canvas drew. So the element is real, the browser owns
//! every behavior GPUI would otherwise reproduce, and the draw exists to put
//! its pixels in the scene's compositing order rather than above it.
//!
//! This module decides whether that path is available and owns the layer that
//! draws it. It is the only place in GPUI that names the proposal's entry
//! points, so an application never reaches them and never learns which path it
//! got.
//!
//! # Why a second canvas
//!
//! The specification's section 17 describes element snapshot to WebGL texture
//! to GPUI scene. This platform cannot take that route. It renders through
//! wgpu (`WebPlatform::new_with_backend(_, WebBackendPreference::WebGl)`), and
//! wgpu owns the canvas's GL context: the proposal's 2D call cannot get a 2D
//! context on a canvas wgpu already holds, and its WebGL call needs the GL
//! texture name behind a `wgpu::Texture`, which wgpu exposes through no public
//! API. Reaching it means transmuting `wgpu-hal` internals across a pinned
//! dependency.
//!
//! So the elements live on a second, transparent canvas stacked over the one
//! wgpu owns, and the browser composites the two. That is the proposal's own
//! demonstrated mechanism, it leaves `WgpuRenderer` untouched, and it keeps
//! this whole file inert on a browser that does not ship the proposal.
//!
//! # Why the decision is the platform's
//!
//! Nothing in the GPUI API changes. A leaf declares what it is through the
//! configuration it already sends, and the same draft, the same events and the
//! same semantic node come back on either path. A browser that ships the
//! proposal and a browser that never does must be indistinguishable above this
//! line, which is only true if nothing above this line is asked.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::PlatformNativeElement;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::{JsCast, JsValue};

/// The proposal's entry points, as a browser actually ships them.
///
/// These are not the names the specification's prose uses, and the difference
/// is not cosmetic. Measured against Chrome 151.0.7922.34 with
/// `--enable-blink-features=CanvasDrawElement`, the shipping surface is
/// `requestPaint` and `layoutSubtree` and `captureElementImage` and
/// `getElementTransform` on `HTMLCanvasElement`, and `drawElementImage` on the
/// 2D context. The prose's `updateElementGeometry` exists under no spelling.
///
/// `getElementTransform` is the geometry seam that replaced it, and it points
/// the other way: the browser reports the element's transform to the page
/// rather than the page declaring the element's rectangle to the browser.
/// Section 16 is satisfied anyway, and by a shorter argument -- see
/// [`paint_layer`].
const REQUEST_PAINT: &str = "requestPaint";
const DRAW_ELEMENT_IMAGE: &str = "drawElementImage";
const GET_ELEMENT_TRANSFORM: &str = "getElementTransform";
const LAYOUT_SUBTREE: &str = "layoutSubtree";

/// The event the browser fires when the layer canvas needs its contents again.
const PAINT_EVENT: &str = "paint";

/// The layer canvas's document id, so an oracle can find it.
pub const ELEMENT_LAYER_ID: &str = "gpui-element-layer";

/// The global a test writes to force one path or the other.
///
/// Section 10 permits a test override and requires that ordinary operation
/// need no user-facing setting. A global read once at window creation is both:
/// invisible to a person, and reachable from a page's own bootstrap before
/// anything has rendered. Deliberately not a URL parameter, which the
/// application's route codec would see and rewrite.
const OVERRIDE_GLOBAL: &str = "__gpui_html_in_canvas";

/// What this browser can do with an element a canvas draws.
///
/// Recorded as four separate answers rather than one boolean because the
/// proposal is still moving and ships in pieces. Chrome 151 is itself such a
/// piece: it has the four below and does not have `placeElement`, the
/// proposal's interactive half. Four answers say which piece a browser is, so
/// a refusal can be read rather than guessed at, and so the summary an oracle
/// reads distinguishes "no proposal at all" from "a version we decline".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HtmlInCanvas {
    /// `HTMLCanvasElement.prototype.requestPaint`. The "something moved"
    /// signal. Without it the layer is only redrawn when the browser decides
    /// to, and a control would lag the layout that moved it.
    requests_paint: bool,
    /// `CanvasRenderingContext2D.prototype.drawElementImage`. The draw itself.
    draws_elements: bool,
    /// `HTMLCanvasElement.prototype.getElementTransform`. The geometry seam:
    /// it reports the transform the browser has for a drawn element, which is
    /// how an oracle confirms that the browser and GPUI agree about where the
    /// control is. Probed rather than called here, because calling it is only
    /// legal inside a paint event.
    reads_transform: bool,
    /// The `layoutsubtree` attribute reflecting. Without it the canvas's
    /// descendants are ordinary fallback content: laid out by nothing, and so
    /// drawn as nothing.
    lays_out_subtree: bool,
}

impl HtmlInCanvas {
    /// Probe the browser.
    ///
    /// Probed on the prototypes rather than on a live context. A canvas has
    /// one context type for its lifetime, so probing a real context would mean
    /// allocating a throwaway canvas and a throwaway context -- a real cost
    /// against a browser's small context budget -- to learn something the
    /// prototype already states.
    pub(crate) fn detect() -> Self {
        Self {
            requests_paint: prototype_has("HTMLCanvasElement", REQUEST_PAINT),
            draws_elements: prototype_has("CanvasRenderingContext2D", DRAW_ELEMENT_IMAGE),
            reads_transform: prototype_has("HTMLCanvasElement", GET_ELEMENT_TRANSFORM),
            lays_out_subtree: prototype_has("HTMLCanvasElement", LAYOUT_SUBTREE),
        }
    }

    /// Section 10's "complete support?" branch.
    ///
    /// All four, because a partial implementation must answer unsupported: a
    /// drawable element whose pixels are never drawn is an invisible control,
    /// and section 10 requires Theorem to work without the feature rather than
    /// to work badly with half of it.
    ///
    /// Each of the four is a member a shipping browser has: keying on a name
    /// the proposal's prose uses but no implementation defines would refuse
    /// the native path on every browser forever, which is a silent, permanent
    /// fallback dressed up as feature detection.
    pub(crate) const fn complete(self) -> bool {
        self.requests_paint && self.draws_elements && self.reads_transform && self.lays_out_subtree
    }
}

/// Which implementation a leaf got.
///
/// Reported, never asked for. An application that branched on this would be
/// the fork section 19 forbids.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Realization {
    /// The browser owns the control's editing mechanics, and the layer draws
    /// the browser's own rendering of it.
    BrowserNative,
    /// GPUI paints the control and drives editing through the hidden input
    /// this platform has always used.
    GpuiPainted,
}

impl Realization {
    pub(crate) const fn is_browser_native(self) -> bool {
        matches!(self, Self::BrowserNative)
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::BrowserNative => "browser-native",
            Self::GpuiPainted => "gpui-painted",
        }
    }
}

/// A test's demand, if it made one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Forced {
    Native,
    Painted,
}

impl Forced {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "browser-native",
            Self::Painted => "gpui-painted",
        }
    }
}

/// The decision, and the evidence behind it, made once per window.
///
/// Once, because capability cannot appear part-way through a session, and
/// because a control that changed implementation under a person's caret would
/// lose the composition they were in the middle of.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NativeElements {
    support: HtmlInCanvas,
    forced: Option<Forced>,
    realization: Realization,
}

impl NativeElements {
    pub(crate) fn decide() -> Self {
        let support = HtmlInCanvas::detect();
        let forced = read_override();
        // Forcing the native path on a browser that cannot draw the element is
        // a demand this platform cannot honour, so it is reported as forced
        // and refused rather than silently producing an invisible control.
        // Section 49's fallback oracle needs the opposite direction, and that
        // one always works.
        let realization = match forced {
            Some(Forced::Painted) => Realization::GpuiPainted,
            Some(Forced::Native) if support.complete() => Realization::BrowserNative,
            Some(Forced::Native) => {
                log::warn!(
                    "html-in-canvas: forced native, but this browser answers {support:?}; painting instead"
                );
                Realization::GpuiPainted
            }
            None if support.complete() => Realization::BrowserNative,
            None => Realization::GpuiPainted,
        };
        Self {
            support,
            forced,
            realization,
        }
    }

    pub(crate) const fn realization(self) -> Realization {
        self.realization
    }

    /// Whether this window realizes `capability` as a real browser element.
    ///
    /// Section 11 lists eight capabilities and this platform has built three:
    /// the editable leaf, and the single-line text family. The other five
    /// answer `false` from their own arm below rather than from a shared
    /// default, so that a capability becoming real is a change to one arm,
    /// and so that the gap is readable in the source rather than inferred
    /// from an array that happens to be empty.
    ///
    /// A capability answers for itself. It does not inherit the editable
    /// leaf's answer, which would report a control as realized the moment the
    /// proposal shipped and would put that control's leaf on a native path
    /// with nothing behind it.
    ///
    /// What `true` claims for the single-line family is narrow enough to be
    /// checked, so it is written down rather than left to the arm. The
    /// platform builds the capability's own element -- `input[type=text]` for
    /// [`PlatformNativeElement::EditableText`], `input[type=search]` for
    /// [`PlatformNativeElement::SearchField`] -- drives it as the focused
    /// editable leaf's IME conduit on the same write discipline as the
    /// textarea, places it from the bounds GPUI publishes and draws it where
    /// it lands, and lets the accessibility mirror claim the focused text
    /// node onto it, so that one element is the node's whole realization
    /// rather than a copy beside one. Which leaf it serves is decided by the
    /// role that leaf publishes into the accessibility tree: the element has
    /// to be the kind of control the node says it is, or a reader meets the
    /// right number of controls of the wrong kind. [`crate::ime_mirror`]
    /// carries the conduit policy, including why a single-line leaf cannot
    /// share the textarea mirror at all.
    pub(crate) const fn realizes(self, capability: PlatformNativeElement) -> bool {
        match capability {
            // Built: the composer's editable leaf, which section 12 asked for
            // first and which section 13's proof is what earned the rest of
            // this list.
            PlatformNativeElement::EditableLeaf => self.realization.is_browser_native(),
            // Built: the single-line text family. The same division of
            // responsibility as the leaf above, on an element the platform
            // must not wrap, so the same question has the same answer.
            PlatformNativeElement::EditableText | PlatformNativeElement::SearchField => {
                self.realization.is_browser_native()
            }
            // Enumerated by section 11 and not built. Answering `false` here
            // keeps each of these on the painted path, which section 19
            // requires to be a real path rather than an untested branch.
            PlatformNativeElement::RichText
            | PlatformNativeElement::NativeButton
            | PlatformNativeElement::NativeCheckbox
            | PlatformNativeElement::NativeSelect
            | PlatformNativeElement::NativeLink => false,
            PlatformNativeElement::None => false,
        }
    }

    /// Build the layer this decision calls for, if it calls for one.
    ///
    /// Routed through the decision rather than constructed beside it so that
    /// one place honours the override: a test that forced the painted path on
    /// a browser that supports the proposal must get no layer, and a check
    /// against raw capability would hand it one.
    pub(crate) fn create_layer(
        self,
        document: &web_sys::Document,
        body: &web_sys::HtmlElement,
    ) -> Option<Rc<ElementLayer>> {
        if !self.realization.is_browser_native() {
            return None;
        }
        ElementLayer::create(document, body, self.support).map(Rc::new)
    }

    pub(crate) fn summary(
        self,
        editable_bounds: Option<[f32; 4]>,
        conduit_element_id: Option<&'static str>,
    ) -> NativeElementSummary {
        NativeElementSummary {
            supported: self.support.complete(),
            requests_paint: self.support.requests_paint,
            draws_elements: self.support.draws_elements,
            reads_transform: self.support.reads_transform,
            lays_out_subtree: self.support.lays_out_subtree,
            forced: self.forced.map(Forced::as_str),
            realization: self.realization.as_str(),
            capabilities: PlatformNativeElement::ALL
                .into_iter()
                .filter(|capability| self.realizes(*capability))
                .map(|capability| capability.name())
                .collect(),
            conduit_element_id,
            editable_bounds,
            last_draw_ms: LAST_DRAW_MS.with(Cell::get),
            last_input_latency_ms: LAST_INPUT_LATENCY_MS.with(Cell::get),
        }
    }
}

/// One element the layer draws, and whether GPUI currently says to show it.
///
/// Visibility is per element rather than per layer. With one drawable the two
/// are the same thing, which is why the layer could hold the flag alone; but
/// section 11 lists eight capabilities, a window may realize more than one of
/// them, and a second drawable with a different visibility needs the flag
/// here.
struct Drawable {
    element: web_sys::HtmlElement,
    visible: bool,
}

/// The transparent canvas the browser's own elements are drawn onto.
///
/// It is stacked over the canvas wgpu owns and composited by the browser, so
/// the renderer never learns it exists. Nothing about GPUI's scene changes; a
/// natively realized leaf simply leaves a hole in it and this layer fills the
/// hole with the browser's rendering of the same control.
pub(crate) struct ElementLayer {
    canvas: web_sys::HtmlCanvasElement,
    /// The drawable elements, in paint order. Their positions are not stored:
    /// each element's own CSS box is where it is, written by whoever owns the
    /// element, and a second copy here would be a second authority for one
    /// rectangle.
    ///
    /// Shared with the paint listener rather than read back off the canvas,
    /// because reading a canvas's children means `HTMLCollection`, a web-sys
    /// feature this crate does not enable, to recover a list this side of the
    /// boundary already has.
    drawables: Rc<RefCell<Vec<Drawable>>>,
    _paint: Closure<dyn FnMut(web_sys::Event)>,
}

impl ElementLayer {
    /// Build the layer, or report that this browser has no use for one.
    ///
    /// `None` is the ordinary answer today and costs nothing: no canvas, no
    /// context, no listener, and every caller below already treats the absence
    /// as "paint it yourself".
    pub(crate) fn create(
        document: &web_sys::Document,
        body: &web_sys::HtmlElement,
        support: HtmlInCanvas,
    ) -> Option<Self> {
        if !support.complete() {
            return None;
        }
        let canvas: web_sys::HtmlCanvasElement = document
            .create_element("canvas")
            .ok()?
            .dyn_into()
            .ok()?;
        canvas.set_id(ELEMENT_LAYER_ID);
        // The attribute, not the IDL property: the property is what was
        // probed, and setting the content attribute is what the proposal's own
        // markup does.
        canvas.set_attribute("layoutsubtree", "").ok()?;
        let style = canvas.style();
        for (name, value) in [
            ("position", "fixed"),
            ("inset", "0"),
            ("width", "100%"),
            ("height", "100%"),
            // The layer must not eat pointer events. A drawable element inside
            // it gets its own hit region from `updateElementGeometry`, which
            // the browser applies when the element is drawn, so the element
            // stays reachable while the empty space around it does not
            // intercept anything aimed at the scene below.
            ("pointer-events", "none"),
        ] {
            style.set_property(name, value).ok()?;
        }
        body.append_child(&canvas).ok()?;

        let drawables: Rc<RefCell<Vec<Drawable>>> = Rc::default();
        let paint_canvas = canvas.clone();
        let paint_drawables = Rc::clone(&drawables);
        let paint = Closure::<dyn FnMut(web_sys::Event)>::new(move |_event: web_sys::Event| {
            paint_layer(&paint_canvas, &paint_drawables.borrow());
        });
        canvas
            .add_event_listener_with_callback(PAINT_EVENT, paint.as_ref().unchecked_ref())
            .ok()?;

        Some(Self {
            canvas,
            drawables,
            _paint: paint,
        })
    }

    /// Move an element into the layer and start drawing it.
    ///
    /// The element becomes a descendant of the layer canvas, which under the
    /// proposal is what makes it laid out but not painted by the page: the
    /// layer draws it instead, exactly once, where GPUI put it.
    pub(crate) fn adopt(&self, element: &web_sys::HtmlElement) {
        if self.canvas.append_child(element).is_err() {
            log::warn!("html-in-canvas: could not adopt an element into the layer");
            return;
        }
        // `pointer-events` inherits, so the layer's `none` would reach the
        // element and make the one control the layer exists for the one
        // control nobody can click. The layer must not intercept; its drawn
        // elements must. Set on adoption rather than where the element is
        // built, because it is the layer's policy that makes it necessary.
        if let Err(error) = element.style().set_property("pointer-events", "auto") {
            log::warn!("html-in-canvas: could not make the element hittable: {error:?}");
        }
        self.drawables.borrow_mut().push(Drawable {
            element: element.clone(),
            visible: true,
        });
        self.request_paint();
    }

    /// Match the drawing surface to the window's.
    ///
    /// A canvas's backing store is 300x150 until something says otherwise, and
    /// CSS only stretches that, so a layer left at the default would draw
    /// every element into a 300x150 image scaled across the viewport. The
    /// layer takes the same physical size the renderer takes, which also makes
    /// the two canvases one coordinate space.
    pub(crate) fn resize(&self, width: u32, height: u32) {
        if self.canvas.width() == width && self.canvas.height() == height {
            return;
        }
        self.canvas.set_width(width);
        self.canvas.set_height(height);
        // Resizing a canvas clears it, so the elements have to be drawn again
        // whether or not they moved.
        self.request_paint();
    }

    /// Ask for a repaint because something moved.
    ///
    /// The layer is otherwise only redrawn when the browser decides to, and a
    /// control that lagged the layout that moved it would smear across a
    /// scroll.
    pub(crate) fn request_paint(&self) {
        call_void(&self.canvas, REQUEST_PAINT, &[]);
    }

    /// Follow GPUI's visibility for one of the layer's elements.
    ///
    /// Section 11 keeps visibility and clipping policy on GPUI's side of the
    /// line, and it has to: the layer composites above the scene rather than
    /// inside it, so an element GPUI had covered with an overlay would
    /// otherwise still show through that overlay.
    ///
    /// Per element, and by the element's own `display`, not by hiding the
    /// layer. Hiding the layer was exact only while there was one drawable,
    /// and a window that realizes two capabilities has two elements with two
    /// independent answers.
    pub(crate) fn set_visible(&self, element: &web_sys::HtmlElement, visible: bool) {
        {
            let mut drawables = self.drawables.borrow_mut();
            let Some(drawable) = drawables
                .iter_mut()
                .find(|drawable| &drawable.element == element)
            else {
                log::warn!("html-in-canvas: asked for the visibility of an element this layer does not draw");
                return;
            };
            if drawable.visible == visible {
                return;
            }
            drawable.visible = visible;
        }
        // Outside the borrow: this is a DOM call, and the paint listener
        // borrows the same list.
        let _ = element
            .style()
            .set_property("display", if visible { "block" } else { "none" });
        // Repaint on the way out as well as the way in. The layer composites
        // above the scene and `drawElementImage` output persists until
        // something repaints, so a hide that skipped this would leave the
        // element's last snapshot -- caret and selection included -- drawn on
        // the layer above whatever overlay just covered it. `forget` and
        // `resize` repaint unconditionally for the same reason.
        self.request_paint();
    }

    /// Stop drawing an element and forget it.
    ///
    /// The element is left where it is: whoever built it owns taking it out of
    /// the document, and doing it here would be a second owner for its
    /// lifetime.
    pub(crate) fn forget(&self, element: &web_sys::HtmlElement) {
        self.drawables
            .borrow_mut()
            .retain(|drawable| &drawable.element != element);
        self.request_paint();
    }

    pub(crate) fn remove(&self) {
        let canvas: &web_sys::Element = self.canvas.as_ref();
        canvas.remove();
    }
}

/// Redraw every drawable the layer holds, at the box it currently occupies.
///
/// Reached only from the browser's own `paint` event, which is the only moment
/// the proposal permits the draw call: outside it the canvas has no valid
/// drawing target for element content.
fn paint_layer(canvas: &web_sys::HtmlCanvasElement, drawables: &[Drawable]) {
    let Ok(Some(context)) = canvas.get_context("2d") else {
        return;
    };
    // SPEC-THEOREMWEB-PWA-HTML-CANVAS-1.1 section 58 tracks "HTML snapshot
    // upload cost" as its own row. This is that cost: the whole of what the
    // browser does to put the element's rendering into the canvas, measured
    // where it happens rather than inferred from a frame time that also
    // contains everything wgpu drew.
    let started = performance_now();
    // `reset` rather than `clearRect`: the proposal's element draws carry
    // state the next frame must not inherit, and a reset is one call instead
    // of a clear plus a transform restore.
    call_void(&context, "reset", &[]);
    // The backing store is in physical pixels and every rectangle below is in
    // CSS pixels, because that is the space GPUI lays out in and the space an
    // element's own box is reported in. One scale reconciles them, and doing
    // it here rather than at each call site means no coordinate is ever
    // converted twice.
    let ratio = web_sys::window().map_or(1.0, |window| window.device_pixel_ratio());
    call_void(
        &context,
        "scale",
        &[JsValue::from_f64(ratio), JsValue::from_f64(ratio)],
    );
    // Only the elements GPUI still says to show. A hidden one is skipped
    // rather than drawn transparent: `display: none` leaves it with a zero
    // box, so the `get_bounding_client_rect` below would draw it at the
    // origin.
    let mut drawn = 0usize;
    for drawable in drawables.iter().filter(|drawable| drawable.visible) {
        let element = &drawable.element;
        drawn += 1;
        // The element's own border box is where GPUI put it, through the
        // inline `left`/`top`/`width`/`height` the IME mirror writes. Asking
        // the browser for that box rather than replaying a stored rectangle
        // keeps one authority for the geometry.
        let rect = element.get_bounding_client_rect();
        // Drawing at the element's own box is what satisfies section 16, and
        // it is worth writing down why, because the obvious reading of the
        // proposal is that a separate call is needed to move the hit region.
        //
        // It is not, and there is no such call: `updateElementGeometry` is
        // prose, and no browser defines it. What a browser does instead is
        // hit-test, focus, route IME to and report to assistive technology the
        // element at its *layout* box -- verified against Chrome 151, where a
        // click inside a drawn copy at (90, 72) reached `body` and a click on
        // the same element's layout box at (60, 12) focused it and accepted
        // typing. The picture a canvas draws is a picture. Input follows CSS.
        //
        // So drawing at `rect` is not one of several defensible choices. It is
        // the only one under which the pixels a person sees and the box the
        // browser routes to are the same rectangle, and it makes them the same
        // rectangle by construction rather than by keeping two in agreement.
        //
        // The layer is `position: fixed; inset: 0`, so its origin is the
        // viewport origin and this viewport-relative rect is already in the
        // canvas's own coordinate space. That is load-bearing: a layer placed
        // any other way would need the difference subtracted here.
        call_void(
            &context,
            DRAW_ELEMENT_IMAGE,
            &[
                element.into(),
                JsValue::from_f64(rect.x()),
                JsValue::from_f64(rect.y()),
            ],
        );
    }
    // Only when something was drawn. A layer with nothing visible costs the
    // reset and the scale, and reporting that as a snapshot cost would put a
    // number near zero against a row that is meant to say what a snapshot
    // costs.
    if drawn > 0
        && let Some(started) = started
        && let Some(now) = performance_now()
    {
        LAST_DRAW_MS.with(|cell| cell.set(Some(now - started)));
    }
}

/// `performance.now()`, or nothing where there is no performance timeline.
fn performance_now() -> Option<f64> {
    web_sys::window()
        .and_then(|window| window.performance())
        .map(|performance| performance.now())
}

/// Call a method the proposal defines and web-sys does not bind.
///
/// A failure is logged and swallowed. Every call site here is a draw or an
/// invalidation on a path that was already probed as supported, so a failure
/// means the browser changed the proposal under us -- worth a line in the
/// console, and not worth taking the window down over.
fn call_void(target: &JsValue, name: &str, arguments: &[JsValue]) {
    let Ok(method) = js_sys::Reflect::get(target, &JsValue::from_str(name)) else {
        return;
    };
    let Ok(method) = method.dyn_into::<js_sys::Function>() else {
        return;
    };
    let call = match arguments {
        [] => method.call0(target),
        [one] => method.call1(target, one),
        [one, two] => method.call2(target, one, two),
        [one, two, three] => method.call3(target, one, two, three),
        _ => {
            let array = js_sys::Array::new();
            for argument in arguments {
                array.push(argument);
            }
            js_sys::Reflect::apply(&method, target, &array)
        }
    };
    if let Err(error) = call {
        log::warn!("html-in-canvas: {name} failed: {error:?}");
    }
}

/// Whether `<global>.prototype` answers to `name`.
///
/// `Reflect::has` is the `in` operator, so it walks the prototype chain and
/// finds a member the browser installed on a base interface. A browser without
/// the constructor at all answers `false` at the first step. Nothing is
/// invoked: a capability probe that calls the capability is a probe that can
/// break the page it is probing.
fn prototype_has(global_name: &str, name: &str) -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    let Ok(constructor) = js_sys::Reflect::get(&window, &JsValue::from_str(global_name)) else {
        return false;
    };
    if constructor.is_undefined() || constructor.is_null() {
        return false;
    }
    let Ok(prototype) = js_sys::Reflect::get(&constructor, &JsValue::from_str("prototype")) else {
        return false;
    };
    let Ok(prototype) = prototype.dyn_into::<js_sys::Object>() else {
        return false;
    };
    js_sys::Reflect::has(&prototype, &JsValue::from_str(name)).unwrap_or(false)
}

fn read_override() -> Option<Forced> {
    let window = web_sys::window()?;
    let value = js_sys::Reflect::get(&window, &JsValue::from_str(OVERRIDE_GLOBAL)).ok()?;
    match value.as_string()?.as_str() {
        "native" => Some(Forced::Native),
        "painted" => Some(Forced::Painted),
        other => {
            log::warn!("html-in-canvas: ignoring {OVERRIDE_GLOBAL}={other:?}");
            None
        }
    }
}

/// What the platform decided and what it saw, for oracles.
///
/// Sections 49, 50 and 51 all need to assert against the path actually taken:
/// a fallback oracle that cannot prove the fallback ran is asserting nothing.
/// This is the same shape as [`crate::A11yMirrorSummary`] and is read the same
/// way, through a host the window publishes into.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NativeElementSummary {
    /// Whether this browser has every entry point the layer needs.
    pub supported: bool,
    pub requests_paint: bool,
    pub draws_elements: bool,
    pub reads_transform: bool,
    pub lays_out_subtree: bool,
    /// The path a test demanded, if one did.
    pub forced: Option<&'static str>,
    /// The path in force.
    pub realization: &'static str,
    /// Which of section 11's eight capabilities this window realizes
    /// natively, by the specification's names.
    ///
    /// One entry per realized capability, so an oracle can assert that a named
    /// capability is realized and that the others are not, rather than only
    /// that *something* was. On the painted path this is empty, which is what
    /// the fallback's assertion reads.
    pub capabilities: Vec<&'static str>,
    /// The editable leaf's last published bounds, in CSS pixels, as
    /// `[x, y, width, height]`. Section 51 compares these against GPUI's own
    /// bounds and against where the browser reports the element.
    pub editable_bounds: Option<[f32; 4]>,
    /// The document id of the element the IME conduit is driving for the
    /// focused editable leaf -- `"gpui-ime-input"` for the multiline leaf,
    /// `"gpui-ime-input-text"` and `"gpui-ime-input-search"` for the
    /// single-line family -- or `None` where no conduit is live.
    ///
    /// An id rather than the capability's delegated element name, because
    /// "which element is live" is a question an oracle asks from JavaScript
    /// and the answer has to be something `getElementById` can be handed.
    ///
    /// A realized capability is not the same statement as the element in
    /// force: a window that realizes three editable capabilities still drives
    /// one element at a time, and section 15's "the reader meets the control
    /// once" is a claim about that element. An oracle that could only read the
    /// realized set could not tell a search field served by a search input
    /// from one served by a textarea, which is the difference this field
    /// exists to make visible. `None` on the painted path, where no element is
    /// drawing anything.
    pub conduit_element_id: Option<&'static str>,
    /// Section 58: the most recent HTML snapshot upload cost, in milliseconds.
    /// `None` until the layer has drawn at least one element, which on the
    /// painted path is never.
    pub last_draw_ms: Option<f64>,
    /// Section 58: the most recent editor input latency, in milliseconds --
    /// from the browser stamping the input event to GPUI having applied it.
    /// `None` until something has been typed.
    pub last_input_latency_ms: Option<f64>,
}

thread_local! {
    /// The live window's report. There is one window on the web and one main
    /// thread, which is what lets an oracle read this without owning it.
    static SUMMARY_SOURCE: RefCell<Option<Rc<dyn Fn() -> NativeElementSummary>>> =
        const { RefCell::new(None) };

    /// Section 58's two HTML-in-Canvas rows, as the most recent sample of
    /// each. The most recent rather than an average, because an average hides
    /// the frame that was slow and section 58's point is that these costs are
    /// tracked separately rather than folded into one another.
    static LAST_DRAW_MS: Cell<Option<f64>> = const { Cell::new(None) };
    static LAST_INPUT_LATENCY_MS: Cell<Option<f64>> = const { Cell::new(None) };
}

/// Record how long the browser took to apply one input to the editor.
///
/// Section 58's "HTML-native editor input latency". Called from the input
/// path, which runs on both realizations; the reader publishes it under that
/// name only where the leaf really is browser-native, and the realization
/// travels in the same report so the two cannot be read apart.
pub(crate) fn record_input_latency(milliseconds: f64) {
    LAST_INPUT_LATENCY_MS.with(|cell| cell.set(Some(milliseconds)));
}

pub(crate) fn publish_summary_source(source: Rc<dyn Fn() -> NativeElementSummary>) {
    SUMMARY_SOURCE.with(|cell| *cell.borrow_mut() = Some(source));
}

/// The current native-element report, for oracles.
pub fn native_element_summary() -> Option<NativeElementSummary> {
    SUMMARY_SOURCE.with(|cell| cell.borrow().as_ref().map(|source| source()))
}
