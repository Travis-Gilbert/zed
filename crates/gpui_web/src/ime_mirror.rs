//! The editable conduits that connect browser IMEs to GPUI.
//!
//! IMEs (software keyboards, composition engines) decide what backspace,
//! autocorrect, and suggestions mean by inspecting the focused editable
//! element's value and selection. This module owns that element -- the
//! *conduit* -- and keeps a window of the document's text mirrored into it, so
//! IME edits arrive as interpretable events instead of operations against an
//! empty field.
//!
//! The element's value and selection are only ever written by [`sync`],
//! reached through [`ImeMirror::schedule_sync`]: every write is observed by
//! the IME and makes the browser restart the IME's input connection, so
//! writes must be coalesced to at most one per browser event-loop turn,
//! landing only after the current gesture's events have all dispatched.
//! Keeping the elements and their write paths private to this module makes
//! that discipline a compile-time guarantee rather than a convention.
//!
//! # One conduit per realized editable capability
//!
//! Section 11 divides an editable leaf: GPUI keeps identity, layout, focus,
//! state, visibility and lifecycle, and the browser owns IME, caret,
//! selection, autocorrect, spellcheck, the virtual keyboard, browser text
//! semantics and native accessibility semantics. Every one of those
//! browser-owned properties is a property of *the focused element*. A caret is
//! not drawn in an element that does not hold focus, IME is routed only to the
//! focused element, and assistive technology follows focus too. So the element
//! that carries them and the element the layer draws have to be the same
//! element, and it has to be the element the capability names.
//!
//! That is why this module holds several elements and exactly one live one.
//! The `<textarea>` is section 11's multiline editable leaf, and it is a
//! textarea on purpose: a single-line input silently strips line breaks from
//! an assigned value, which would make the mirror text disagree with what was
//! written into it and desynchronize the diff that imports an IME edit. A
//! single-line capability therefore cannot share that element -- a textarea
//! wrapping free is the wrong control for a leaf the platform must not wrap,
//! and its browser text semantics, which are the role a reader hears, what
//! Enter does and whether a search field offers search affordances, are the
//! wrong ones -- and it cannot be a second element beside the mirror either:
//! the unfocused one would carry no caret, and section 15's "a screen reader
//! must encounter the control exactly once" would meet two controls, the
//! focused one of the wrong kind.
//!
//! The conduit is typed by the capability of the focused editable leaf, and
//! all of them are driven through one discipline: the multiline leaf on the
//! textarea, `EditableText` and `SearchField` on `input[type=text]` and
//! `input[type=search]`. The capability is read from the role the leaf
//! publishes for itself into the accessibility tree, which is the same fact
//! one step later: that role is what the accessibility mirror would project
//! for the node, and section 15 requires the real element to *be* that
//! projection rather than a second one beside it. [`ImeMirror::capability_in_force`]
//! holds the policy, and the gate in [`crate::native_element`] governs it: a
//! conduit is only ever used for a capability this window answers for.
//!
//! The single-line conduits exist only where the window is browser-native. On
//! the painted path this module builds one conduit, the invisible textarea
//! this platform has always used, and never a second element: section 19's
//! fallback is a path rather than a branch of this one.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{Autocapitalize, PlatformNativeElement, TextInputAction, TextInputConfiguration};
use wasm_bindgen::JsCast;

use crate::native_element::{ElementLayer, NativeElements};
use crate::window::{IME_INPUT_ELEMENT_ID, WebWindowInner};

/// UTF-16 code units of document text mirrored on each side of the
/// selection.
///
/// There is no demand-driven protocol to size this against: an IME's
/// context requests (e.g. Android's `getTextBeforeCursor`) are answered by
/// the browser from the element's current state, invisibly to the page, so
/// the window must be provisioned ahead of time. The lower bound is what
/// IMEs actually read — sentence-scale context, on the order of a hundred
/// units. The upper bound is that the window size is a per-keystroke cost,
/// not a one-time cost: every imported edit diffs the element's full value
/// against the stored mirror, and every rebuild writes the full window into
/// the element and re-snapshots it across the browser–IME boundary, all on
/// the main thread between frames. An IME that would read further than the
/// window simply sees text truncated at the window's edge — the same thing
/// it sees near the start of any short field — so oversizing buys nothing.
const CONTEXT_CHARS: usize = 512;

/// The element is left alone until the selection gets this close to the
/// mirrored window's edge (unless it desynchronizes outright). Must exceed
/// the span an IME plausibly reads or edits around the caret within a
/// single gesture (a long word plus autocorrect lookback); beyond that,
/// recentering lazily is strictly better, because every recenter is an
/// element write and therefore an IME restart.
const MIN_EDGE_CHARS: usize = 64;

/// What the browser would draw of a native conduit *except* the caret and the
/// selection.
///
/// Section 11 gives the browser caret, selection, IME, autocorrect, spellcheck
/// and the virtual keyboard, and gives GPUI the text. `color: transparent` is
/// the load-bearing entry: it suppresses the platform's duplicate glyphs while
/// leaving the selection highlight, which paints behind text, and the caret,
/// whose color is a separate property. The element stays a real, focused,
/// hit-testable control throughout, so nothing about IME changes.
const NATIVE_STYLE: [(&str, &str); 8] = [
    ("background", "transparent"),
    ("border", "none"),
    ("outline", "none"),
    ("padding", "0"),
    ("margin", "0"),
    ("resize", "none"),
    ("overflow", "hidden"),
    ("color", "transparent"),
];

/// The document id of the element realizing `capability`.
///
/// One id per capability rather than one for the family, because an oracle has
/// to be able to say which element is live, and because two elements sharing
/// an id would make `getElementById` answer whichever came first in the tree.
/// The multiline leaf keeps [`IME_INPUT_ELEMENT_ID`], which is the id this
/// platform has always published for it.
const fn conduit_element_id(capability: PlatformNativeElement) -> &'static str {
    match capability {
        PlatformNativeElement::EditableText => "gpui-ime-input-text",
        PlatformNativeElement::SearchField => "gpui-ime-input-search",
        _ => IME_INPUT_ELEMENT_ID,
    }
}

/// The `type` a single-line conduit's element carries, or `None` for the
/// multiline leaf, which is not an input at all.
const fn conduit_input_type(capability: PlatformNativeElement) -> Option<&'static str> {
    match capability {
        PlatformNativeElement::EditableText => Some("text"),
        PlatformNativeElement::SearchField => Some("search"),
        _ => None,
    }
}

/// The element behind a conduit, with the operations the mirror needs of it.
///
/// The two kinds differ in exactly one thing that matters here: whether the
/// browser may change a value on the way in. Everything else -- value,
/// selection, read-only, focus -- is the same protocol on both.
enum ConduitElement {
    /// The multiline editable leaf: free to hold any window of the document,
    /// line breaks included.
    TextArea(web_sys::HtmlTextAreaElement),
    /// A single-line capability: an input, whose value sanitization strips
    /// line breaks from an assigned value.
    Input(web_sys::HtmlInputElement),
}

impl ConduitElement {
    fn as_element(&self) -> &web_sys::HtmlElement {
        let element: &web_sys::HtmlElement = match self {
            Self::TextArea(element) => element.as_ref(),
            Self::Input(element) => element.as_ref(),
        };
        element
    }

    fn value(&self) -> String {
        match self {
            Self::TextArea(element) => element.value(),
            Self::Input(element) => element.value(),
        }
    }

    fn set_value(&self, value: &str) {
        match self {
            Self::TextArea(element) => element.set_value(value),
            Self::Input(element) => element.set_value(value),
        }
    }

    fn selection_start(&self) -> Option<u32> {
        match self {
            Self::TextArea(element) => element.selection_start().ok().flatten(),
            Self::Input(element) => element.selection_start().ok().flatten(),
        }
    }

    fn selection_end(&self) -> Option<u32> {
        match self {
            Self::TextArea(element) => element.selection_end().ok().flatten(),
            Self::Input(element) => element.selection_end().ok().flatten(),
        }
    }

    fn set_selection_range(&self, start: u32, end: u32) {
        match self {
            Self::TextArea(element) => element.set_selection_range(start, end).ok(),
            Self::Input(element) => element.set_selection_range(start, end).ok(),
        };
    }

    fn read_only(&self) -> bool {
        match self {
            Self::TextArea(element) => element.read_only(),
            Self::Input(element) => element.read_only(),
        }
    }

    fn set_read_only(&self, read_only: bool) {
        match self {
            Self::TextArea(element) => element.set_read_only(read_only),
            Self::Input(element) => element.set_read_only(read_only),
        }
    }

    /// Whether this element can hold `text` exactly as written.
    ///
    /// The single-line answer is the reason this module holds more than one
    /// element and the reason a write is checked before it happens: an input
    /// strips line breaks from an assigned value *silently*, so an unchecked
    /// write would leave the element holding text the mirror believes it
    /// wrote, and every later diff -- which is how an IME edit becomes a
    /// document edit -- would be computed against text that is not there.
    fn holds(&self, text: &str) -> bool {
        match self {
            Self::TextArea(_) => true,
            Self::Input(_) => !text.contains(['\n', '\r']),
        }
    }
}

/// One element the platform may drive for the focused editable leaf.
struct Conduit {
    /// The capability this element realizes, and so the only capability it
    /// may serve.
    capability: PlatformNativeElement,
    element: ConduitElement,
}

impl Conduit {
    fn as_element(&self) -> &web_sys::HtmlElement {
        self.element.as_element()
    }
}

/// The elements IMEs edit, plus the bookkeeping that relates the live one to
/// the document.
///
/// The elements and every value/selection write on them are private to this
/// module; other code interacts through read accessors, focus and read-only
/// control, and [`ImeMirror::schedule_sync`].
pub(crate) struct ImeMirror {
    /// Every element this window may drive for the focused editable leaf, the
    /// textarea first. Exactly one of them is live: the one whose capability
    /// the focused leaf declares and this window answers for.
    ///
    /// On the painted path this holds the textarea alone.
    conduits: Vec<Conduit>,
    /// The capability whose conduit is live -- in the document, focused,
    /// drawn and claimed. Starts at the multiline leaf, which is what this
    /// platform drives until a leaf declares otherwise.
    live: Cell<PlatformNativeElement>,
    /// The live conduit's element, shared with the accessibility adapter so
    /// that a conduit switch moves the claim with the focus instead of
    /// leaving it on an element that is no longer the control.
    live_element: Rc<RefCell<web_sys::HtmlElement>>,
    /// The capability the focused node publishes for itself, written by the
    /// accessibility adapter on every frame that carries the tree.
    ///
    /// A cell rather than a query, because the tree arrives on its own
    /// schedule and the conduit settles when bounds or a sync arrive. A stale
    /// value can only be the previous frame's answer, and the next settle
    /// corrects it.
    declared: Rc<Cell<PlatformNativeElement>>,
    /// The last configuration the application forwarded, so a conduit that
    /// becomes live after it arrives carries the focused leaf's text
    /// assistance rather than the defaults.
    configuration: RefCell<TextInputConfiguration>,
    /// Set once, the first time a fetched window has to be narrowed to the
    /// caret's line because the live conduit cannot hold a line break, so a
    /// leaf that hands multiline text to a single-line capability is reported
    /// once instead of on every sync.
    refused_line_break: Cell<bool>,
    /// The layer that draws these elements, on the browser-native path only.
    /// Holding it here is what lets a move publish a repaint: the browser has
    /// no reason to redraw a canvas because a descendant's style changed.
    layer: Option<Rc<ElementLayer>>,
    /// What this window can realize. The gate holds a conduit back from a
    /// capability the platform does not answer for, so a switch can never
    /// move the input path onto an element the platform has not built.
    native_elements: NativeElements,
    /// The editable leaf's last published bounds in CSS pixels, for section
    /// 51's geometry oracle. Held here rather than recomputed from the
    /// element's style, because what the oracle must check is that the
    /// browser agrees with the bounds GPUI *sent*.
    published_bounds: Cell<Option<[f32; 4]>>,
    /// The mirror text most recently synced to (or observed in) the hidden
    /// element. `input` events diff the element's new value against this to
    /// recover what edit the IME performed.
    text: RefCell<String>,
    /// The element's selection (in element-local UTF-16 offsets) as of the
    /// last sync or imported edit. Gives IME edits their position relative
    /// to the caret; deliberately element-local, never document
    /// coordinates, which go stale in a collaborative document.
    selection: Cell<(u32, u32)>,
    /// Document offset where the mirror window starts — as a *hint only*.
    /// It is never trusted for edits: every use first re-verifies the
    /// stored window text against the document at this alignment, so a
    /// stale hint costs a window rebuild instead of a misplaced edit.
    window_hint: Cell<usize>,
    /// Whether a coalesced sync is already scheduled for the next task.
    /// Multiple sync requests within one gesture must collapse into a
    /// single element write after the gesture: keyboards sample the field
    /// between writes, and a mid-gesture barrage desynchronizes their word
    /// model.
    sync_scheduled: Cell<bool>,
    /// Whether the `selectionchange` import saw an element selection move
    /// it could not apply (the app selection changed underneath). Sync
    /// normally defers to a pending import when the element's live
    /// selection has moved; a rejected import means no import is coming,
    /// so the next sync must reassert the app's state instead of waiting.
    selection_import_rejected: Cell<bool>,
}

/// Whether the device's primary pointer is coarse (a touch screen). The
/// distinction drives virtual-keyboard policy: touch-first browsers summon
/// the keyboard for any focused editable element on a user gesture.
fn primary_pointer_is_coarse() -> bool {
    web_sys::window()
        .and_then(|window| window.match_media("(pointer: coarse)").ok().flatten())
        .is_some_and(|media_query_list| media_query_list.matches())
}

impl ImeMirror {
    pub(crate) fn new(
        document: &web_sys::Document,
        body: &web_sys::HtmlElement,
        layer: Option<Rc<ElementLayer>>,
        native_elements: NativeElements,
    ) -> anyhow::Result<Self> {
        // A textarea rather than an input: single-line inputs silently strip
        // newlines from assigned values, which would make the mirror text
        // disagree with what was written into it. The single-line capabilities
        // get elements of their own for that reason, and only where this
        // window is browser-native; see this module's header for which node
        // the browser draws and which one carries the IME.
        let element: web_sys::HtmlTextAreaElement = document
            .create_element("textarea")
            .map_err(|e| anyhow::anyhow!("Failed to create textarea element: {e:?}"))?
            .dyn_into()
            .map_err(|e| anyhow::anyhow!("Created element is not a textarea: {e:?}"))?;
        element.set_id(IME_INPUT_ELEMENT_ID);
        let style = element.style();
        // Both paths place the element in viewport coordinates, because the
        // canvas fills the viewport: one `update_position` then serves both,
        // and the caret bounds GPUI sends need no second transform.
        style.set_property("position", "fixed").ok();
        style.set_property("top", "0").ok();
        style.set_property("left", "0").ok();
        style.set_property("width", "1px").ok();
        style.set_property("height", "1px").ok();
        // Android Chrome zooms the visual viewport onto a focused text input
        // whose font is smaller than 16px; with page zoom disabled the user
        // can never zoom back out, so keep the hidden IME input at 16px.
        style.set_property("font-size", "16px").ok();
        if let Some(layer) = layer.as_deref() {
            // Section 11 divides the editable leaf: GPUI keeps the content and
            // the browser contributes caret, selection, spellcheck and
            // autocorrect. A textarea's default appearance contributes more
            // than that -- an opaque background, a border, a focus ring and a
            // second copy of the text in the user agent's font -- and on this
            // path those are drawn over the scene GPUI just painted.
            //
            // So everything the browser would draw *except* the caret and the
            // selection is turned off; [`NATIVE_STYLE`] is that list and says
            // which entry is load-bearing. The element stays a real, focused,
            // hit-testable textarea throughout, so nothing about IME changes.
            for (name, value) in NATIVE_STYLE {
                style.set_property(name, value).ok();
            }
            // Into the element layer, not into the canvas wgpu owns. A
            // canvas's element children are its fallback content, which the
            // HTML specification already says is laid out but never painted,
            // and which the HTML-in-Canvas proposal makes drawable -- but only
            // the canvas that draws them can draw them, and wgpu holds the
            // other canvas's context for its own output. So the element's
            // parent is the transparent layer stacked above the scene, and the
            // browser composites the two.
            layer.adopt(element.as_ref());
        } else {
            // Painted: the element exists only to hold the IME's attention,
            // so it is one transparent pixel that nothing can see and nothing
            // can reach.
            style.set_property("opacity", "0").ok();
            body.append_child(&element)
                .map_err(|e| anyhow::anyhow!("Failed to append input to body: {e:?}"))?;
        }
        element.focus().ok();
        // The element must stay focused to receive hardware-key and IME
        // events, but on touch-first devices a focused *editable* element
        // invites the browser to summon the virtual keyboard on the next
        // user gesture — including a scroll. Start read-only there; only a
        // recognized tap on text input lifts it (`sync_virtual_keyboard`).
        if primary_pointer_is_coarse() {
            element.set_read_only(true);
        }

        let mut conduits = vec![Conduit {
            capability: PlatformNativeElement::EditableLeaf,
            element: ConduitElement::TextArea(element),
        }];
        // The single-line conduits are built where this window is
        // browser-native, and built here rather than on first use, because the
        // events module attaches one set of listeners per conduit when the
        // window is created: an element that appeared later would be an
        // element whose input, composition and key events reach nobody. Each
        // is adopted and then hidden, which is what keeps it out of the
        // document's accessibility tree and out of the layer's draws until it
        // is the live one.
        //
        // The gate decides whether the element exists at all, so
        // `NativeElements::realizes` answering `false` and this element not
        // being here are the same statement rather than two that agree.
        if let Some(layer) = layer.as_deref() {
            for capability in [
                PlatformNativeElement::EditableText,
                PlatformNativeElement::SearchField,
            ] {
                if !native_elements.realizes(capability) {
                    continue;
                }
                conduits.push(Self::single_line(document, layer, capability)?);
            }
        }
        let live_element = Rc::new(RefCell::new(conduits[0].as_element().clone()));
        let this = Self {
            conduits,
            live: Cell::new(PlatformNativeElement::EditableLeaf),
            live_element,
            declared: Rc::new(Cell::new(PlatformNativeElement::None)),
            configuration: RefCell::new(TextInputConfiguration::default()),
            refused_line_break: Cell::new(false),
            layer,
            native_elements,
            published_bounds: Cell::new(None),
            text: RefCell::new(String::new()),
            selection: Cell::new((0, 0)),
            window_hint: Cell::new(0),
            sync_scheduled: Cell::new(false),
            selection_import_rejected: Cell::new(false),
        };
        // Until an input handler asks otherwise, the element is an IME
        // conduit, not a form field: browser-side text assistance would
        // mutate it behind the app's back.
        this.apply_configuration(&TextInputConfiguration::default());
        Ok(this)
    }

    /// Build one single-line conduit and adopt it into the layer, hidden.
    ///
    /// The element is the capability's own: section 11's single-line editable
    /// text is `input[type=text]` and its search field is
    /// `input[type=search]`. An element of the wrong kind is the wrong browser
    /// text semantics even when its caret works, and that is the half a reader
    /// and a software keyboard both notice.
    fn single_line(
        document: &web_sys::Document,
        layer: &ElementLayer,
        capability: PlatformNativeElement,
    ) -> anyhow::Result<Conduit> {
        let element: web_sys::HtmlInputElement = document
            .create_element("input")
            .map_err(|e| anyhow::anyhow!("Failed to create an input element: {e:?}"))?
            .dyn_into()
            .map_err(|e| anyhow::anyhow!("Created element is not an input: {e:?}"))?;
        element.set_id(conduit_element_id(capability));
        // The attribute rather than the IDL property, and set before the
        // element is in the document, so the browser builds the input the
        // capability names rather than re-typing one it already made.
        if let Some(input_type) = conduit_input_type(capability) {
            element.set_attribute("type", input_type).ok();
        }
        let style = element.style();
        // The same placement the textarea gets, and for the same reason: both
        // paths place the element in viewport coordinates, so one
        // `update_position` can serve every conduit. The size here is
        // provisional -- the element is hidden until a leaf publishes bounds,
        // and those bounds are what it is given.
        style.set_property("position", "fixed").ok();
        style.set_property("top", "0").ok();
        style.set_property("left", "0").ok();
        style.set_property("width", "1px").ok();
        style.set_property("height", "1px").ok();
        style.set_property("font-size", "16px").ok();
        for (name, value) in NATIVE_STYLE {
            style.set_property(name, value).ok();
        }
        layer.adopt(element.as_ref());
        layer.set_visible(element.as_ref(), false);
        let conduit = Conduit {
            capability,
            element: ConduitElement::Input(element),
        };
        Ok(conduit)
    }

    /// Maps a [`TextInputConfiguration`] onto the live conduit's
    /// text-assistance attributes, and remembers it for whichever conduit
    /// becomes live next. Callers must only invoke this on actual
    /// configuration changes (GPUI diffs before forwarding): mutating the
    /// focused element can restart the IME's input connection.
    pub(crate) fn apply_configuration(&self, configuration: &TextInputConfiguration) {
        *self.configuration.borrow_mut() = configuration.clone();
        self.apply_configuration_to(&self.live_conduit(), configuration);
    }

    /// The same, for one named conduit.
    ///
    /// A conduit that becomes live mid-session is given the last
    /// configuration, because GPUI forwards one only when it changes and the
    /// leaf that is now focused may well want what the leaf before it wanted.
    fn apply_configuration_to(&self, conduit: &Conduit, configuration: &TextInputConfiguration) {
        let element: &web_sys::Element = conduit.as_element().as_ref();
        conduit
            .as_element()
            .set_spellcheck(configuration.suggestions);
        let on_off = |enabled: bool| if enabled { "on" } else { "off" };
        element
            .set_attribute("autocomplete", on_off(configuration.suggestions))
            .ok();
        element
            .set_attribute("autocorrect", on_off(configuration.autocorrect))
            .ok();
        element
            .set_attribute(
                "autocapitalize",
                match configuration.autocapitalize {
                    Autocapitalize::None => "off",
                    Autocapitalize::Words => "words",
                    Autocapitalize::Sentences => "sentences",
                    Autocapitalize::Characters => "characters",
                },
            )
            .ok();
        let enter_key_hint = match configuration.input_action {
            TextInputAction::Unspecified => None,
            TextInputAction::Enter => Some("enter"),
            TextInputAction::Done => Some("done"),
            TextInputAction::Go => Some("go"),
            TextInputAction::Next => Some("next"),
            TextInputAction::Previous => Some("previous"),
            TextInputAction::Search => Some("search"),
            TextInputAction::Send => Some("send"),
        };
        match enter_key_hint {
            Some(hint) => element.set_attribute("enterkeyhint", hint).ok(),
            None => element.remove_attribute("enterkeyhint").ok(),
        };
    }

    /// The element a reader should meet for the focused node, and the cell
    /// that moves with it when the conduit switches.
    ///
    /// The accessibility adapter holds this rather than an element, because
    /// the element that is the node's realization is whichever conduit is
    /// live, and section 15's "the control exactly once" is a claim about that
    /// one: a claim left behind on the textarea while a search input is the
    /// control would leave the reader with a focused multiline box and an
    /// unfocused search field for one node.
    pub(crate) fn accessibility_element_handle(&self) -> Rc<RefCell<web_sys::HtmlElement>> {
        Rc::clone(&self.live_element)
    }

    /// The cell the accessibility adapter publishes the focused node's
    /// capability into.
    ///
    /// It is the adapter that reads it, because the role AGPUI gives a node is
    /// what the adapter already claims the node on; giving the mirror the same
    /// fact keeps the element and the claim from being two answers to one
    /// question.
    pub(crate) fn declared_capability_handle(&self) -> Rc<Cell<PlatformNativeElement>> {
        Rc::clone(&self.declared)
    }

    /// Browser caret bounds use CSS pixels, matching GPUI's logical coordinates.
    ///
    /// Section 16 makes GPUI the layout authority on both paths, so both take
    /// the bounds GPUI prepainted and neither lets the browser decide where
    /// the control sits. They differ in width, and for a reason: a painted
    /// mirror is one pixel wide because a wider invisible textarea would
    /// swallow pointer events over the control GPUI drew, while a
    /// browser-native element must occupy the control's real box or the
    /// browser lays out its text, hit-tests its caret and reports its
    /// accessibility geometry against the wrong rectangle.
    ///
    /// They also differ in *how* they move, and that is the whole difficulty
    /// of the native path. `layoutsubtree` gives the canvas layout authority
    /// over its descendants, and it exercises it: measured against Chrome
    /// 153.0.8010.12, a child of such a canvas ignores `left` and `top` under
    /// `fixed`, `absolute` and `relative` alike, and ignores `margin` too. It
    /// honours `width` and `height`, and every child lands on the canvas's
    /// origin stacked on the last.
    ///
    /// A transform is applied after layout, so it is the one channel the
    /// canvas does not consume. `transform: translate(x, y)` moves the
    /// element, `getBoundingClientRect` reports the moved box, and
    /// `elementFromPoint` and a real click both land on it there. So the
    /// native path translates and the painted path offsets, and section 16
    /// holds on both.
    /// Settle the conduit the bounds belong to, then give it the bounds.
    ///
    /// The capability in force can change with the focus, and a bound
    /// published for one capability is a bound for the element that serves it.
    /// Settling first means the element about to be placed is the one that
    /// will hold focus, be drawn, and be claimed for the focused node.
    pub(crate) fn update_position(
        &self,
        window: &WebWindowInner,
        bounds: gpui::Bounds<gpui::Pixels>,
    ) {
        // A conduit is never swapped mid-composition. The composition belongs
        // to the element that is holding it, and section 18 forbids anything
        // here that would destroy it; the next bounds or sync after the
        // composition ends settles whatever was pending.
        if !window.is_composing.get() {
            self.settle_conduit(window);
        }
        let conduit = self.live_conduit();
        let x = f32::from(bounds.origin.x);
        let y = f32::from(bounds.origin.y);
        let width = f32::from(bounds.size.width).max(1.0);
        let height = f32::from(bounds.size.height).max(1.0);
        self.place(conduit, x, y, width, height);
        self.published_bounds.set(Some([x, y, width, height]));
        // GPUI prepainted a caret box, so there is a live editable leaf and
        // it is here. That is both the visibility signal and the repaint
        // signal: the element moved, so its pixels are in the wrong place
        // until the layer draws again, and nothing else would ask -- a canvas
        // is not invalidated by a descendant's layout.
        if let Some(layer) = self.layer.as_deref() {
            layer.set_visible(conduit.as_element(), true);
            layer.request_paint();
        }
    }

    /// Write one conduit's box, in the channel the canvas does not consume.
    fn place(&self, conduit: &Conduit, x: f32, y: f32, width: f32, height: f32) {
        let style = conduit.as_element().style();
        let mut properties = vec![
            ("left", format!("{x}px")),
            ("top", format!("{y}px")),
            ("height", format!("{height}px")),
        ];
        if self.native_elements.realization().is_browser_native() {
            // `left` and `top` were still written above, because they cost
            // nothing and a browser that later lays this subtree out the
            // ordinary way would then place the element correctly without a
            // second code path. The transform is what moves it today.
            properties.push(("width", format!("{width}px")));
            properties.push(("transform", format!("translate({x}px, {y}px)")));
        }
        for (name, value) in properties {
            if let Err(error) = style.set_property(name, &value) {
                log::warn!("Failed to position IME mirror {name}: {error:?}");
            }
        }
    }

    pub(crate) fn reset_position(&self) {
        let conduit = self.live_conduit();
        let style = conduit.as_element().style();
        let mut properties = vec![("left", "0"), ("top", "0"), ("height", "1px")];
        if self.native_elements.realization().is_browser_native() {
            properties.push(("width", "1px"));
            properties.push(("transform", "none"));
        }
        for (name, value) in properties {
            if let Err(error) = style.set_property(name, value) {
                log::warn!("Failed to reset IME mirror {name}: {error:?}");
            }
        }
        self.published_bounds.set(None);
        // Focus left the leaf, so GPUI says there is nothing to draw. Section
        // 11 keeps that judgement on GPUI's side, and it has to: the layer
        // composites above the scene, so an element the renderer had covered
        // would otherwise still show through whatever GPUI drew over it.
        if let Some(layer) = self.layer.as_deref() {
            layer.set_visible(conduit.as_element(), false);
        }
    }

    /// The capability whose conduit should be live.
    ///
    /// The focused leaf's own answer, where this window has the element for
    /// it, and the multiline leaf otherwise -- which is what this platform
    /// drove before there was more than one element, so a window whose leaves
    /// declare nothing keeps the behavior the composer proof was taken on. The
    /// gate has the last word: a capability this window does not answer for
    /// never gets an element to be served from, whatever a node declares.
    fn capability_in_force(&self) -> PlatformNativeElement {
        let declared = self.declared.get();
        let declared_is_served = declared != PlatformNativeElement::None
            && self.native_elements.realizes(declared)
            && self
                .conduits
                .iter()
                .any(|conduit| conduit.capability == declared);
        if declared_is_served {
            declared
        } else {
            PlatformNativeElement::EditableLeaf
        }
    }

    /// The conduit in the document, focused, drawn and claimed.
    fn live_conduit(&self) -> &Conduit {
        let live = self.live.get();
        self.conduits
            .iter()
            .find(|conduit| conduit.capability == live)
            // The textarea is built unconditionally and is the first conduit,
            // so this is unreachable; it exists so that the accessors below
            // cannot fail open on a missing element.
            .unwrap_or(&self.conduits[0])
    }

    /// The capability whose element is driving the focused editable leaf.
    pub(crate) fn live_capability(&self) -> PlatformNativeElement {
        self.live_conduit().capability
    }

    /// The document id of the element driving the focused editable leaf, for
    /// the capability report.
    ///
    /// An id rather than the element, because "which element is live" is a
    /// question an oracle asks from JavaScript, where the answer has to be
    /// something `getElementById` can be handed. `None` on the painted path,
    /// which has no element of its own to name.
    pub(crate) fn live_conduit_id(&self) -> Option<&'static str> {
        self.native_elements
            .realization()
            .is_browser_native()
            .then(|| conduit_element_id(self.live_capability()))
    }

    /// Make the live conduit the one the focused leaf calls for, if it is not
    /// already.
    ///
    /// A switch moves DOM focus, the layer's draws and the accessibility claim
    /// together, because all three belong to whichever element is live. Focus
    /// moves before the old element is hidden, so the platform's focus is
    /// never on an element that is going away and never on the document body:
    /// the browser sends the next key to whatever holds focus, and the keydown
    /// path this platform lives by is a listener on the conduit. The move is
    /// bracketed as a suppressed focus change, because a conduit swap is not
    /// the window becoming inactive.
    fn settle_conduit(&self, window: &WebWindowInner) {
        let wanted = self.capability_in_force();
        let live = self.live.get();
        if wanted == live {
            return;
        }
        let Some(next) = self
            .conduits
            .iter()
            .find(|conduit| conduit.capability == wanted)
        else {
            return;
        };
        let previous = self
            .conduits
            .iter()
            .find(|conduit| conduit.capability == live);
        window.suppress_focus_status_events.set(true);
        // Focus first, and stop here if it does not take. Everything below
        // assumes the browser is delivering input to the element that becomes
        // live -- that is the whole reason the conduits share a node with
        // their IME -- and a switch that moved the claim while leaving focus
        // behind would draw and claim an element that no keystroke reaches.
        // The old conduit stays live and the next settle tries again.
        if next.as_element().focus().is_err() {
            window.suppress_focus_status_events.set(false);
            return;
        }
        if let Some(previous) = previous {
            // Blur cannot fail in a way that matters here: the element is
            // losing focus either way, and the switch is already committed to
            // the new one.
            let _ = previous.as_element().blur();
            if let Some(layer) = self.layer.as_deref() {
                layer.set_visible(previous.as_element(), false);
            }
        }
        window.suppress_focus_status_events.set(false);
        if let Some(layer) = self.layer.as_deref() {
            layer.set_visible(next.as_element(), true);
        }
        // The new element holds none of the document yet. Until the next sync
        // writes a window into it, every diff and every test of whether the
        // element is still an accurate mirror would be answered against text
        // that belongs to the element that just left, so the bookkeeping is
        // reset and the next write is a rebuild.
        self.text.borrow_mut().clear();
        self.selection.set((0, 0));
        self.window_hint.set(0);
        // The attributes the focused leaf's configuration names have to arrive
        // with the element, since GPUI forwards a configuration only when it
        // changes and the leaf before this one may have been the one that
        // forwarded it.
        let configuration = self.configuration.borrow().clone();
        self.apply_configuration_to(next, &configuration);
        self.live_element.replace(next.as_element().clone());
        self.live.set(wanted);
        // Section 16: the geometry GPUI published is the geometry this element
        // has, so the element that becomes live takes the bounds that were
        // published for the leaf it serves rather than waiting for the next
        // frame to say so.
        if let Some([x, y, width, height]) = self.published_bounds.get() {
            self.place(next, x, y, width, height);
        }
        if let Some(layer) = self.layer.as_deref() {
            layer.request_paint();
        }
    }

    /// The bounds last sent to the element, for section 51's oracle.
    pub(crate) fn published_bounds(&self) -> Option<[f32; 4]> {
        self.published_bounds.get()
    }

    /// Every element whose events belong to the platform's input path.
    ///
    /// The events module registers one set of listeners per conduit, because
    /// input, key and composition events fire on the focused element and do
    /// not travel to a sibling: a listener set that stayed on the textarea
    /// would leave a live search input typing into nothing.
    pub(crate) fn event_targets(&self) -> Vec<web_sys::EventTarget> {
        self.conduits
            .iter()
            .map(|conduit| {
                let target: &web_sys::EventTarget = conduit.as_element().as_ref();
                target.clone()
            })
            .collect()
    }

    pub(crate) fn focus(&self) {
        self.live_conduit().as_element().focus().ok();
    }

    pub(crate) fn is_focused(&self) -> bool {
        let element: &web_sys::Element = self.live_conduit().as_element().as_ref();
        web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.active_element())
            .is_some_and(|active| &active == element)
    }

    pub(crate) fn blur(&self) {
        self.live_conduit().as_element().blur().ok();
    }

    pub(crate) fn read_only(&self) -> bool {
        self.live_conduit().element.read_only()
    }

    pub(crate) fn set_read_only(&self, read_only: bool) {
        self.live_conduit().element.set_read_only(read_only);
    }

    pub(crate) fn remove(&self) {
        // Told to the layer before the elements go: a layer still holding a
        // detached element would draw it at the origin on the next paint.
        for conduit in &self.conduits {
            if let Some(layer) = self.layer.as_deref() {
                layer.forget(conduit.as_element());
            }
            let element: &web_sys::Element = conduit.as_element().as_ref();
            element.remove();
        }
    }

    pub(crate) fn value(&self) -> String {
        self.live_conduit().element.value()
    }

    pub(crate) fn selection_start(&self) -> Option<u32> {
        self.live_conduit().element.selection_start()
    }

    pub(crate) fn element_selection_end(&self) -> Option<u32> {
        self.live_conduit().element.selection_end()
    }

    pub(crate) fn stored_text(&self) -> String {
        self.text.borrow().clone()
    }

    pub(crate) fn stored_selection(&self) -> (u32, u32) {
        self.selection.get()
    }

    /// Adopts the element's current value and selection as the mirror
    /// baseline without writing to the element. Used when the browser
    /// itself applied an edit (an imported IME edit, a composition commit):
    /// the element is already what the IME expects, and echoing a write
    /// back would restart the IME mid-gesture.
    pub(crate) fn adopt_element_state(&self) {
        let conduit = self.live_conduit();
        *self.text.borrow_mut() = conduit.element.value();
        let selection_start = self.selection_start().unwrap_or(0);
        let selection_end = conduit.element.selection_end().unwrap_or(selection_start);
        self.selection.set((selection_start, selection_end));
    }

    /// Records that an element selection move could not be imported, so
    /// the next sync reasserts the app's state rather than deferring to an
    /// import that is no longer coming.
    pub(crate) fn reject_selection_import(&self) {
        self.selection_import_rejected.set(true);
    }

    /// The window of the document to mirror, fitted to what the live
    /// conduit can actually hold.
    ///
    /// A single-line control holds one line and strips anything else from an
    /// assigned value *silently*. Writing a window that spans a line break
    /// would therefore leave the element holding text this module believes it
    /// wrote, and every later diff -- which is how an IME edit becomes a
    /// document edit -- would be computed against text that is not in the
    /// element. The damage is not a stale mirror: the missing text reads as a
    /// deletion, so the next keystroke would delete a line of the document
    /// the user never touched. So the window becomes the line the caret is
    /// on, which is the most a native field of that kind ever mirrors.
    ///
    /// For the single-line capabilities this platform realizes, the leaf's
    /// document is normally one line, so this is usually a no-op that returns
    /// the window it was given. Where it is not, the app has handed multiline
    /// text to a single-line control and the narrowing is reported once.
    ///
    /// The multiline conduit holds anything, so it never narrows and the
    /// proven path is unchanged. Nor does narrowing cost a write by itself:
    /// the write below is guarded by equality against the element's value, so
    /// a window that narrows to the same line as last time writes nothing.
    fn fit_to_conduit(
        &self,
        conduit: &Conduit,
        window_start: usize,
        text: String,
        app_selection_start: usize,
    ) -> (usize, String) {
        if conduit.element.holds(&text) {
            return (window_start, text);
        }
        let caret = app_selection_start.saturating_sub(window_start);
        let (line_start, line) = line_containing(&text, caret);
        if !self.refused_line_break.replace(true) {
            log::warn!(
                "A single-line native element cannot hold a line break; the mirror is \
                 following the caret's line instead of the surrounding window"
            );
        }
        (window_start + line_start, line.to_owned())
    }

    /// Schedules a coalesced sync of the mirror for the next task.
    ///
    /// Event handlers must not write to the mirror element mid-gesture:
    /// every write (value, selection) is observed by the IME, and a
    /// sequence of writes inside one gesture desynchronizes its model of
    /// the field (every native-behaving reference — a plain textarea —
    /// performs at most one such change per gesture). Deferring to a
    /// zero-delay timeout coalesces all sync requests from one gesture into
    /// a single write that lands after the browser has finished processing
    /// the gesture's events.
    ///
    /// `sync` is deliberately nested here so that scheduling is the only
    /// way to reach it: a direct synchronous call would reintroduce the
    /// mid-gesture writes this indirection exists to prevent.
    pub(crate) fn schedule_sync(window: &Rc<WebWindowInner>) {
        if window.ime_mirror.sync_scheduled.replace(true) {
            return;
        }
        let closure = wasm_bindgen::closure::Closure::once_into_js({
            let window = Rc::clone(window);
            move || {
                window.ime_mirror.sync_scheduled.set(false);
                sync(&window);
            }
        });
        window
            .browser_window
            .set_timeout_with_callback(closure.unchecked_ref())
            .ok();

        /// Mirrors the text surrounding the selection into the hidden
        /// element.
        ///
        /// With an empty element, Gboard deletes against its private buffer
        /// (the keypress reaches the page only as an `"Unidentified"`
        /// placeholder) and its suggestion strip has no context. Mirroring
        /// a window of real text makes those operations arrive as
        /// interpretable `beforeinput` events.
        ///
        /// All offsets are UTF-16 code units on both sides: GPUI's
        /// input-handler protocol and JavaScript string indexing agree by
        /// construction.
        ///
        /// Writing to the element is a last resort: any rewrite of its
        /// value or selection makes the browser restart the IME's input
        /// connection, which resets the keyboard's state — fatal in the
        /// middle of a keyboard's multi-step edit sequence (suggestion
        /// picks arrive as delete-then-insert pairs). After an imported
        /// edit, the element already *is* a faithful — if off-center —
        /// window of the document, so this first verifies the element
        /// against the document at its current alignment and skips every
        /// write while that holds. The window is rebuilt only when the app
        /// changed independently (caret moved by tap or keybinding, remote
        /// edit inside the window) or the selection drifted too close to
        /// the window's edge to give the IME context.
        fn sync(window: &WebWindowInner) {
            if window.is_composing.get() {
                return;
            }
            let mirror = &window.ime_mirror;
            // Everything below reads and writes *the live conduit*: the
            // element the focused leaf's capability called for, which is the
            // one holding focus and the one the browser is delivering this
            // gesture to. Naming it once keeps a sync from straddling two
            // elements if the capability changes while one is in flight.
            let conduit = mirror.live_conduit();
            // A live element selection that differs from the stored baseline
            // while the value still matches is an IME-driven selection move
            // whose `selectionchange` import hasn't dispatched yet (the event
            // is asynchronous, and this sync may run first). The element owns
            // the selection until that import runs: writing now would clobber
            // an in-progress gesture, e.g. Android's slide-on-backspace
            // growing its selection. The import reconciles the two sides and
            // schedules a fresh sync when it cannot adopt the move.
            if !mirror.selection_import_rejected.replace(false)
                && *mirror.text.borrow() == conduit.element.value()
            {
                let live_start = mirror.selection_start().unwrap_or(0);
                let live_end = mirror.element_selection_end().unwrap_or(live_start);
                if (live_start, live_end) != mirror.selection.get() {
                    return;
                }
            }
            let selection = window
                .with_input_handler(|handler| handler.selected_text_range(false))
                .flatten();
            let Some(selection) = selection else {
                // An empty value is the one value every conduit holds.
                if !mirror.text.borrow().is_empty() {
                    conduit.element.set_value("");
                    mirror.text.borrow_mut().clear();
                }
                mirror.selection.set((0, 0));
                return;
            };
            // The mirrored window never crosses the handler's editable
            // range: everything in the element is reachable by multi-step
            // IME edit gestures (word deletion, autocorrect rewrites), so
            // text outside the range must not be mirrored at all. The IME
            // sees the range's edges as the field's edges.
            let editable_range = window
                .with_input_handler(|handler| handler.text_input_editable_range())
                .flatten();

            if is_consistent(
                window,
                &selection.range,
                editable_range.as_ref(),
                MIN_EDGE_CHARS,
            ) {
                return;
            }

            // A caret move within the existing window (a tap into nearby
            // text) must update only the element's selection, like a native
            // tap in a plain textarea. Rewriting the value restarts the IME
            // connection, which desynchronizes the keyboard's word model
            // right when it is about to act on the tapped word.
            if move_selection_within_window(
                window,
                &selection.range,
                editable_range.as_ref(),
                MIN_EDGE_CHARS,
            ) {
                return;
            }

            let mut window_range = selection.range.start.saturating_sub(CONTEXT_CHARS)
                ..selection.range.end + CONTEXT_CHARS;
            if let Some(editable_range) = &editable_range {
                window_range.start = window_range.start.max(editable_range.start);
                window_range.end = window_range
                    .end
                    .min(editable_range.end)
                    .max(window_range.start);
            }
            let mut adjusted = None;
            let text = window
                .with_input_handler(|handler| {
                    handler.text_for_range(window_range.clone(), &mut adjusted)
                })
                .flatten()
                .unwrap_or_default();
            let window_start = adjusted.unwrap_or(window_range).start;
            let (window_start, text) =
                mirror.fit_to_conduit(&conduit, window_start, text, selection.range.start);

            if *mirror.text.borrow() != text || conduit.element.value() != text {
                conduit.element.set_value(&text);
                *mirror.text.borrow_mut() = text;
            }

            mirror.window_hint.set(window_start);
            let selection_start = selection.range.start.saturating_sub(window_start) as u32;
            let selection_end = selection.range.end.saturating_sub(window_start) as u32;
            if conduit.element.selection_start() != Some(selection_start)
                || conduit.element.selection_end() != Some(selection_end)
            {
                conduit
                    .element
                    .set_selection_range(selection_start, selection_end);
            }
            // Read the selection back rather than trusting the computed
            // values: the browser clamps out-of-bounds positions, and a
            // stored selection the element doesn't actually have would
            // corrupt the next diff.
            let actual_start = conduit.element.selection_start();
            let actual_end = conduit.element.selection_end();
            mirror.selection.set((
                actual_start.unwrap_or(selection_start),
                actual_end.unwrap_or(selection_end),
            ));
        }
    }
}

/// The line-break-free segment of `text` that contains the caret, as
/// `(start, line)` in UTF-16 code units from `text`'s start.
///
/// Offsets here are UTF-16 units on both sides, matching every other offset on
/// this path, and the slicing is done on character boundaries so a surrogate
/// pair is never split. A caret past the end of `text` is treated as sitting
/// at the end, and `"\r\n"` counts as two breaks: the segment between them is
/// empty, which is a window every element holds, so the degenerate case
/// degrades to a degraded IME rather than to a wrong document.
fn line_containing(text: &str, caret: usize) -> (usize, &str) {
    let caret = caret.min(text.encode_utf16().count());
    let mut units = 0usize;
    let mut line_start_units = 0usize;
    let mut line_start_byte = 0usize;
    for (byte, character) in text.char_indices() {
        if character == '\n' || character == '\r' {
            // The line that ends here is complete, and lies before the caret
            // unless the caret is on this break or before it.
            if caret <= units {
                return (line_start_units, &text[line_start_byte..byte]);
            }
            line_start_units = units + character.len_utf16();
            line_start_byte = byte + character.len_utf8();
        }
        units += character.len_utf16();
    }
    (line_start_units, &text[line_start_byte..])
}

/// Attempts to represent a changed app selection as a pure element
/// selection move within the existing mirror window.
///
/// The stored window-start hint is re-verified textually against the
/// document before use, so a stale hint (remote edit, any drift) fails
/// verification and falls through to a full window rebuild rather than
/// mispositioning the selection.
fn move_selection_within_window(
    window: &WebWindowInner,
    app_selection: &std::ops::Range<usize>,
    editable_range: Option<&std::ops::Range<usize>>,
    min_edge: usize,
) -> bool {
    let mirror = &window.ime_mirror;
    let stored_text = mirror.text.borrow().clone();
    let stored_length = stored_text.encode_utf16().count();
    if stored_length == 0 || mirror.value() != stored_text {
        return false;
    }
    let window_start = mirror.window_hint.get();

    // The new selection must sit inside the window with enough context
    // on both sides — except where the window is pinned to a boundary (of
    // the document or of the editable range), where less context is all
    // the context there is. This is the common case: a chat thread's
    // caret usually sits at the end of the document, where the window has
    // no right margin at all.
    // A window that leaks outside the editable range mirrors text the IME
    // must not reach, however consistent it is.
    if let Some(range) = editable_range
        && (window_start < range.start || window_start + stored_length > range.end)
    {
        return false;
    }
    let left_boundary = editable_range.map_or(0, |range| range.start);
    let Some(selection_start) = app_selection.start.checked_sub(window_start) else {
        return false;
    };
    let selection_end = selection_start + (app_selection.end - app_selection.start);
    if selection_end > stored_length {
        return false;
    }
    if selection_start < min_edge && window_start > left_boundary {
        return false;
    }

    // Verify the hint: the stored window text must still equal the
    // document at this alignment. Asking for one unit extra also
    // determines whether the window reaches the document's end, which
    // excuses a missing right margin, as does reaching the editable
    // range's end.
    let mut adjusted = None;
    let document_text = window
        .with_input_handler(|handler| {
            handler.text_for_range(
                window_start..window_start + stored_length + 1,
                &mut adjusted,
            )
        })
        .flatten()
        .unwrap_or_default();
    let document_text_length = document_text.encode_utf16().count();
    let window_at_right_boundary = document_text_length == stored_length
        || editable_range.is_some_and(|range| window_start + stored_length >= range.end);
    if selection_end + min_edge > stored_length && !window_at_right_boundary {
        return false;
    }
    if !document_text.starts_with(stored_text.as_str()) || document_text_length > stored_length + 1
    {
        return false;
    }

    let conduit = mirror.live_conduit();
    conduit
        .element
        .set_selection_range(selection_start as u32, selection_end as u32);
    let actual_start = conduit.element.selection_start();
    let actual_end = conduit.element.selection_end();
    if actual_start != Some(selection_start as u32) || actual_end != Some(selection_end as u32) {
        return false;
    }
    mirror
        .selection
        .set((selection_start as u32, selection_end as u32));
    true
}

/// Whether the hidden element, at its current window alignment, is still
/// an accurate mirror of the document around the app selection with
/// enough context on both sides. When this holds, a sync must not touch
/// the element (see [`sync`] on why writes are harmful).
fn is_consistent(
    window: &WebWindowInner,
    app_selection: &std::ops::Range<usize>,
    editable_range: Option<&std::ops::Range<usize>>,
    min_edge: usize,
) -> bool {
    let mirror = &window.ime_mirror;
    let (element_selection_start, element_selection_end) = mirror.selection.get();
    let element_selection_start = element_selection_start as usize;
    let element_selection_end = element_selection_end as usize;
    let stored_text = mirror.text.borrow().clone();
    let stored_length = stored_text.encode_utf16().count();

    if stored_length == 0 {
        return false;
    }
    // The element's real selection must match what we believe it is.
    if mirror.selection_start() != Some(element_selection_start as u32)
        || mirror.element_selection_end() != Some(element_selection_end as u32)
    {
        return false;
    }
    // Enough context on both sides of the selection, unless the window
    // is pinned to a boundary of the document or of the editable range
    // (start of window at the left boundary, or window end at the right
    // boundary — the document's approximated by the stored window being
    // shorter than requested on that side).
    let app_window_start = match app_selection.start.checked_sub(element_selection_start) {
        Some(start) => start,
        None => return false,
    };
    // A window that leaks outside the editable range mirrors text the IME
    // must not reach, however consistent it is.
    if let Some(range) = editable_range
        && (app_window_start < range.start || app_window_start + stored_length > range.end)
    {
        return false;
    }
    let left_boundary = editable_range.map_or(0, |range| range.start);
    let has_left_context = element_selection_start >= min_edge || app_window_start <= left_boundary;
    let right_context = stored_length.saturating_sub(element_selection_end);
    if !has_left_context || right_context < min_edge {
        let window_end = app_window_start + stored_length;
        let at_right_boundary = if let Some(range) = editable_range {
            window_end >= range.end
        } else {
            // Verify the window genuinely reaches the end of the document
            // by asking for one unit past the stored window.
            let mut adjusted = None;
            window
                .with_input_handler(|handler| {
                    handler.text_for_range(window_end..window_end + 1, &mut adjusted)
                })
                .flatten()
                .unwrap_or_default()
                .is_empty()
        };
        if !has_left_context || !at_right_boundary {
            return false;
        }
    }
    // The stored window must still equal the document at this alignment
    // (a remote edit inside the window invalidates it), and the element
    // must still hold exactly the stored text.
    let mut adjusted = None;
    let document_text = window
        .with_input_handler(|handler| {
            handler.text_for_range(
                app_window_start..app_window_start + stored_length,
                &mut adjusted,
            )
        })
        .flatten()
        .unwrap_or_default();
    if document_text != stored_text {
        return false;
    }
    if mirror.value() != stored_text {
        return false;
    }
    // The element selection corresponds to the app selection end too?
    app_selection.end.checked_sub(app_window_start) == Some(element_selection_end)
}
