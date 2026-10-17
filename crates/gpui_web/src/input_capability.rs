//! DOM capability carried by the current canonical input session.
//!
//! The native handler's FocusGained/FocusLost notifications control read-only
//! state on the live conduit. An inactive DOM node may retain an older state;
//! a handoff must carry the current session's answer before attempting focus.
use wasm_bindgen::JsCast as _;

pub(crate) fn read_only(element: &web_sys::HtmlElement) -> bool {
    if let Some(element) = element.dyn_ref::<web_sys::HtmlTextAreaElement>() {
        element.read_only()
    } else if let Some(element) = element.dyn_ref::<web_sys::HtmlInputElement>() {
        element.read_only()
    } else {
        element.content_editable() == "false"
    }
}

pub(crate) fn set_read_only(element: &web_sys::HtmlElement, read_only: bool) {
    if let Some(element) = element.dyn_ref::<web_sys::HtmlTextAreaElement>() {
        element.set_read_only(read_only);
    } else if let Some(element) = element.dyn_ref::<web_sys::HtmlInputElement>() {
        element.set_read_only(read_only);
    } else {
        element.set_content_editable(if read_only { "false" } else { "plaintext-only" });
    }
}

pub(crate) fn inherit_read_only(current: &web_sys::HtmlElement, successor: &web_sys::HtmlElement) {
    set_read_only(successor, read_only(current));
}

pub(crate) fn ensure_styled_focusability(element: &web_sys::HtmlElement) {
    // Read-only textarea/input conduits remain focusable for selection and
    // shortcuts. contenteditable=false alone would make a retained div unable
    // to regain focus and report the native owner's later focus intent.
    // Respect an explicit tab order supplied by the page.
    if !element.has_attribute("tabindex") {
        element.set_tab_index(0);
    }
}

/// A built conduit temporarily yields its platform claim to a presented island.
/// Preserve the element's prior policy; dropping the claim restores only the
/// attributes this handoff owns, without changing focus or edit capability.
pub(crate) struct RetiredInputClaim {
    element: web_sys::HtmlElement,
    aria_hidden: Option<String>,
    tabindex: Option<String>,
}

impl RetiredInputClaim {
    pub(crate) fn retire(element: &web_sys::HtmlElement) -> Self {
        let claim = Self {
            element: element.clone(),
            aria_hidden: element.get_attribute("aria-hidden"),
            tabindex: element.get_attribute("tabindex"),
        };
        claim.reconcile();
        claim
    }

    pub(crate) fn reconcile(&self) {
        set_attribute(&self.element, "aria-hidden", Some("true"));
        set_attribute(&self.element, "tabindex", Some("-1"));
    }
}

impl Drop for RetiredInputClaim {
    fn drop(&mut self) {
        set_attribute(&self.element, "aria-hidden", self.aria_hidden.as_deref());
        set_attribute(&self.element, "tabindex", self.tabindex.as_deref());
    }
}

fn set_attribute(element: &web_sys::HtmlElement, name: &str, value: Option<&str>) {
    if element.get_attribute(name).as_deref() == value {
        return;
    }
    match value {
        Some(value) => {
            element.set_attribute(name, value).ok();
        }
        None => {
            element.remove_attribute(name).ok();
        }
    }
}

/// Host admission uses explicit hidden/inert ancestry while an island warms.
/// Logical input ownership alone must not remove the native fallback's claim.
/// This checks declared admission facts, without polling computed layout.
pub(crate) fn is_presented(element: &web_sys::HtmlElement) -> bool {
    element.is_connected()
        && element
            .closest("[hidden], [inert], [aria-hidden=\"true\"]")
            .ok()
            .flatten()
            .is_none()
}

/// Whether the built conduit is only transport for the offered island or a
/// non-text focus target. A different actual text leaf keeps its own claim.
pub(crate) fn is_passive_builtin(island_node: Option<u64>, focused_text: Option<u64>) -> bool {
    island_node.is_some() && (focused_text.is_none() || focused_text == island_node)
}

/// Identity facts projected by the existing accessibility text-entry mapping.
/// Binding is per adoption: a later different field must never be rebound as
/// the same island merely because the conduit handoff has not settled yet.
#[derive(Default)]
pub(crate) struct TextInputClaimIdentity {
    island: std::cell::Cell<Option<u64>>,
    focused: std::cell::Cell<Option<u64>>,
}

impl TextInputClaimIdentity {
    pub(crate) fn reset(&self) {
        self.island.set(None);
    }

    pub(crate) fn observe(&self, focused: Option<u64>, current_island_association: bool) {
        self.focused.set(focused);
        if current_island_association && self.island.get().is_none() {
            self.island.set(focused);
        }
    }

    pub(crate) fn is_passive_builtin(&self) -> bool {
        is_passive_builtin(self.island.get(), self.focused.get())
    }
}
