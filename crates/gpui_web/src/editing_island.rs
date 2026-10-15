//! A page-owned, visible text field adopted as the window's IME conduit.
//!
//! The page creates and places the element; GPUI never moves, restyles, or
//! renames it. While the island is live it receives exactly the listeners
//! the hidden conduit has, so input, selection, composition, paste, and
//! history reach the focused input handler through the one import path, and
//! the mirror writes the handler's document back to it as the smallest
//! replacement. The input handler stays the only owner of the text, the
//! selection, the composition, and the history: the element is a view of
//! them that the browser may edit.

use std::{cell::RefCell, rc::Weak};

use crate::{events::EventListenerHandle, ime_mirror::EditingIslandCounts, window::WebWindowInner};

thread_local! {
    static WINDOW: RefCell<Weak<WebWindowInner>> = const { RefCell::new(Weak::new()) };
}

/// Record the window that islands adopted on this thread attach to.
pub(crate) fn publish_window(window: Weak<WebWindowInner>) {
    WINDOW.with(|slot| *slot.borrow_mut() = window);
}

/// An adopted editing island. Dropping it releases the element back to the
/// page and returns the window to its hidden conduit.
pub struct EditingIsland {
    window: Weak<WebWindowInner>,
    element: web_sys::HtmlTextAreaElement,
    adoption: u64,
    _listeners: Vec<EventListenerHandle>,
}

impl EditingIsland {
    /// Adopt `element` as the window's editing island, or `None` when no
    /// window has opened on this thread or an IME composition is active.
    /// Refusal leaves the predecessor and its listeners untouched.
    pub fn adopt(element: web_sys::HtmlTextAreaElement) -> Option<Self> {
        let window = WINDOW.with(|slot| slot.borrow().clone());
        let inner = window.upgrade()?;
        if inner.is_composing.get() {
            return None;
        }
        let adoption = inner.ime_mirror.adopt_island(&inner, element.clone());
        let listeners = inner.attach_input_listeners(&element, adoption);
        Some(Self {
            window,
            element,
            adoption,
            _listeners: listeners,
        })
    }

    /// The adopted element.
    pub fn element(&self) -> &web_sys::HtmlTextAreaElement {
        &self.element
    }

    /// Make the island the live conduit (`true`) or return input to the
    /// hidden conduit (`false`). Returns whether the island is now live; a
    /// change is refused while a composition is open, because moving the
    /// conduit would end it without a commit. A replaced handle returns false
    /// and cannot change its successor, even when it adopts the same element.
    pub fn set_live(&self, live: bool) -> bool {
        let Some(inner) = self.window.upgrade() else {
            return false;
        };
        if !inner
            .ime_mirror
            .owns_island_adoption(&self.element, self.adoption)
        {
            return false;
        }
        if inner.is_composing.get() && live != inner.ime_mirror.island_is_live() {
            return inner.ime_mirror.island_is_live();
        }
        let changed = inner.ime_mirror.set_island_live(&inner, live);
        if changed && inner.ime_mirror.island_is_live() {
            crate::ime_mirror::ImeMirror::schedule_sync(&inner);
        }
        inner.ime_mirror.island_is_live()
    }

    /// Let plain Tab and Shift-Tab from this live textarea use browser focus
    /// order. The hidden conduit retains GPUI traversal. An open composition
    /// still owns those keys. Returns false after this island was replaced
    /// or its window closed; a predecessor cannot configure its successor.
    pub fn set_native_tab_navigation(&self, enabled: bool) -> bool {
        self.window.upgrade().is_some_and(|inner| {
            inner
                .ime_mirror
                .set_island_native_tab(&self.element, self.adoption, enabled)
        })
    }

    /// Schedule the existing coalesced mirror reconciliation after the
    /// canonical editor changes independently of browser input. Call on a
    /// revision or selection change, rather than on every rendered frame.
    /// Composition keeps its current buffer until its normal commit/cancel.
    pub fn request_sync(&self) {
        if let Some(inner) = self.window.upgrade() {
            if inner
                .ime_mirror
                .owns_island_adoption(&self.element, self.adoption)
                && inner.ime_mirror.island_is_live()
            {
                crate::ime_mirror::ImeMirror::schedule_sync(&inner);
            }
        }
    }

    /// Whether this adoption is still the live conduit.
    pub fn is_live(&self) -> bool {
        self.window.upgrade().is_some_and(|inner| {
            inner
                .ime_mirror
                .owns_island_adoption(&self.element, self.adoption)
                && inner.ime_mirror.island_is_live()
        })
    }

    /// Whether a composition is open on the window.
    pub fn composing(&self) -> bool {
        self.window
            .upgrade()
            .is_some_and(|inner| inner.is_composing.get())
    }

    /// Whether the platform currently holds any text input handler. This is
    /// presence only, not evidence that it belongs to this editor. `None`
    /// means the window is gone or its state is temporarily borrowed.
    pub fn input_handler_present(&self) -> Option<bool> {
        let inner = self.window.upgrade()?;
        let state = inner.state.try_borrow().ok()?;
        Some(state.input_handler.is_some())
    }

    /// What this adoption has done. A replaced handle returns zero counts.
    pub fn counts(&self) -> EditingIslandCounts {
        self.window
            .upgrade()
            .filter(|inner| {
                inner
                    .ime_mirror
                    .owns_island_adoption(&self.element, self.adoption)
            })
            .map(|inner| inner.ime_mirror.island_counts())
            .unwrap_or_default()
    }
}

impl Drop for EditingIsland {
    fn drop(&mut self) {
        if let Some(inner) = self.window.upgrade() {
            inner
                .ime_mirror
                .release_island(&inner, &self.element, self.adoption);
        }
    }
}
