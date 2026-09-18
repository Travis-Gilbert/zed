//! Browser-native realization of a GPUI leaf.
//!
//! A GPUI leaf is ordinarily painted: its contents become vertices and
//! textures in the scene, and every behavior a person expects of the control
//! is GPUI's to reproduce. For an editable leaf that reproduction is where the
//! browser costs the most, because a browser does not offer caret, selection,
//! IME, autocorrect, spellcheck or the virtual keyboard as primitives. It
//! offers them only as properties of real elements.
//!
//! The HTML-in-Canvas proposal closes that gap: a real element, laid out by
//! the browser, drawn where the application wants it, and hit-tested there.
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

use wasm_bindgen::prelude::Closure;
use wasm_bindgen::{JsCast, JsValue};

/// The proposal's entry points.
///
/// These are the names the proposal actually ships, which are not quite the
/// names the specification's prose uses: the draw call is `drawElementImage`
/// on the 2D context, the invalidation signal is `requestPaint` on the canvas,
/// and the element's layout participation is the `layoutsubtree` content
/// attribute reflected as `layoutSubtree`.
const REQUEST_PAINT: &str = "requestPaint";
const DRAW_ELEMENT_IMAGE: &str = "drawElementImage";
const UPDATE_ELEMENT_GEOMETRY: &str = "updateElementGeometry";
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
/// proposal is still moving, and a browser that ships the draw call before the
/// geometry call is a real intermediate state. Treating that as support would
/// place a control the browser cannot hit-test, which is worse than painting
/// it: a person would see the element and be unable to click it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HtmlInCanvas {
    /// `HTMLCanvasElement.prototype.requestPaint`. The "something moved"
    /// signal. Without it the layer is only redrawn when the browser decides
    /// to, and a control would lag the layout that moved it.
    requests_paint: bool,
    /// `CanvasRenderingContext2D.prototype.drawElementImage`. The draw itself.
    draws_elements: bool,
    /// `HTMLCanvasElement.prototype.updateElementGeometry`. Tells the browser
    /// where the canvas drew the element, which is what gives it a hit region
    /// and an accessibility rectangle at the drawn position rather than at its
    /// own layout box.
    places_elements: bool,
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
            places_elements: prototype_has("HTMLCanvasElement", UPDATE_ELEMENT_GEOMETRY),
            lays_out_subtree: prototype_has("HTMLCanvasElement", LAYOUT_SUBTREE),
        }
    }

    /// Section 10's "complete support?" branch.
    ///
    /// All four, because a partial implementation must answer unsupported: a
    /// drawable element whose pixels are never drawn is an invisible control,
    /// and section 10 requires Theorem to work without the feature rather than
    /// to work badly with half of it.
    pub(crate) const fn complete(self) -> bool {
        self.requests_paint && self.draws_elements && self.places_elements && self.lays_out_subtree
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

    pub(crate) fn summary(self, editable_bounds: Option<[f32; 4]>) -> NativeElementSummary {
        NativeElementSummary {
            supported: self.support.complete(),
            requests_paint: self.support.requests_paint,
            draws_elements: self.support.draws_elements,
            places_elements: self.support.places_elements,
            lays_out_subtree: self.support.lays_out_subtree,
            forced: self.forced.map(Forced::as_str),
            realization: self.realization.as_str(),
            editable_bounds,
        }
    }
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
    drawables: Rc<RefCell<Vec<web_sys::HtmlElement>>>,
    /// Whether GPUI currently says the leaf is visible. Section 11 keeps
    /// visibility and clipping policy on GPUI's side of the line, and it has
    /// to: a GPUI overlay drawn above the leaf would otherwise still show the
    /// browser's element through it, because the layer composites above the
    /// scene rather than inside it.
    visible: Cell<bool>,
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

        let drawables: Rc<RefCell<Vec<web_sys::HtmlElement>>> = Rc::default();
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
            visible: Cell::new(true),
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
        self.drawables.borrow_mut().push(element.clone());
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

    /// Follow GPUI's visibility for the leaf.
    ///
    /// Hiding the whole layer rather than one element is exact while there is
    /// one drawable and is the reason section 12 says not to generalize to
    /// many controls before the composer proof holds: a second drawable with a
    /// different visibility needs this to become per element.
    pub(crate) fn set_visible(&self, visible: bool) {
        if self.visible.replace(visible) == visible {
            return;
        }
        let _ = self
            .canvas
            .style()
            .set_property("display", if visible { "block" } else { "none" });
        if visible {
            self.request_paint();
        }
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
fn paint_layer(canvas: &web_sys::HtmlCanvasElement, drawables: &[web_sys::HtmlElement]) {
    let Ok(Some(context)) = canvas.get_context("2d") else {
        return;
    };
    // `reset` rather than `clearRect`: the proposal's element draws carry
    // state the next frame must not inherit, and a reset is one call instead
    // of a clear plus a transform restore.
    call_void(&context, "reset", &[]);
    for element in drawables {
        // The element's own border box is where GPUI put it. Asking the
        // browser rather than replaying a stored rectangle keeps one
        // authority for the geometry, and it is the same box the browser will
        // hit-test once `updateElementGeometry` names it.
        let rect = element.get_bounding_client_rect();
        call_void(
            &context,
            DRAW_ELEMENT_IMAGE,
            &[
                element.into(),
                JsValue::from_f64(rect.x()),
                JsValue::from_f64(rect.y()),
            ],
        );
        // Section 16: GPUI stays the layout authority, and this is the call
        // that makes the browser agree. Without it the element is hit-tested
        // and reported to assistive technology at its own layout box inside
        // the canvas, not at the box it was drawn into.
        call_void(
            canvas,
            UPDATE_ELEMENT_GEOMETRY,
            &[
                element.into(),
                JsValue::from_f64(rect.x()),
                JsValue::from_f64(rect.y()),
                JsValue::from_f64(rect.width()),
                JsValue::from_f64(rect.height()),
            ],
        );
    }
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
    pub places_elements: bool,
    pub lays_out_subtree: bool,
    /// The path a test demanded, if one did.
    pub forced: Option<&'static str>,
    /// The path in force.
    pub realization: &'static str,
    /// The editable leaf's last published bounds, in CSS pixels, as
    /// `[x, y, width, height]`. Section 51 compares these against GPUI's own
    /// bounds and against where the browser reports the element.
    pub editable_bounds: Option<[f32; 4]>,
}

thread_local! {
    /// The live window's report. There is one window on the web and one main
    /// thread, which is what lets an oracle read this without owning it.
    static SUMMARY_SOURCE: RefCell<Option<Rc<dyn Fn() -> NativeElementSummary>>> =
        const { RefCell::new(None) };
}

pub(crate) fn publish_summary_source(source: Rc<dyn Fn() -> NativeElementSummary>) {
    SUMMARY_SOURCE.with(|cell| *cell.borrow_mut() = Some(source));
}

/// The current native-element report, for oracles.
pub fn native_element_summary() -> Option<NativeElementSummary> {
    SUMMARY_SOURCE.with(|cell| cell.borrow().as_ref().map(|source| source()))
}
