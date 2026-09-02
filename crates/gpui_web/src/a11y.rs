//! Web accessibility adapter.
//!
//! Every other platform hands GPUI's per-frame [`TreeUpdate`] to an AccessKit
//! platform adapter. The web has no such adapter, because the browser *is* the
//! accessibility platform: assistive technology reads the DOM. A canvas has no
//! DOM, so a canvas application is invisible to a screen reader.
//!
//! This adapter closes that gap by projecting the tree GPUI already builds into
//! a hidden DOM subtree beside the canvas: one element per node, carrying the
//! ARIA role, name, value, state and absolute bounds so assistive technology
//! hit-tests where the pixels actually are. The subtree is `pointer-events:
//! none` throughout, so ordinary pointer input still lands on the canvas, while
//! an AT-synthesized `click()` or `focus()` still reaches our listeners and is
//! dispatched back as an [`accesskit::Action`].
//!
//! Focus ownership does not move. The hidden input that owns keyboard focus
//! keeps it, and `aria-activedescendant` on that input tracks
//! [`TreeUpdate::focus`] -- which is the same active-descendant model GPUI uses
//! internally, so the two agree by construction rather than by convention.
//!
//! The mirror is diffed against the previous frame, so a steady frame writes
//! nothing to the DOM. That is what makes it affordable to leave on rather than
//! gate behind a "screen reader detected" heuristic the web cannot answer.
//!
//! Upstream note: GPUI emits a *complete* tree every frame
//! (`A11yNodeBuilder::finalize` always sets `tree: Some(..)` and pushes every
//! node), so `accesskit_consumer` -- whose purpose is applying incremental
//! updates to a retained tree -- would have nothing to do here. The retained
//! `nodes` map below is the diff, and it holds the DOM element beside the values
//! last written to it so a comparison is one struct compare, not a tree
//! walk.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::accesskit::{self, Action, ActionRequest, Live, NodeId, Rect, Role, Toggled, TreeUpdate};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

/// The attribute that carries a node's AccessKit id on its mirror element.
///
/// One delegated listener on the container reads this rather than each element
/// owning a closure, so node churn costs no allocations.
const NODE_ATTRIBUTE: &str = "data-gpui-a11y-node";

/// The id prefix for mirror elements. `aria-activedescendant` needs a document
/// id to point at, so every mirrored node has one.
const NODE_ID_PREFIX: &str = "gpui-a11y-";

fn element_id(node_id: NodeId) -> String {
    format!("{NODE_ID_PREFIX}{}", node_id.0)
}

fn node_id_from_element(element: &web_sys::Element) -> Option<NodeId> {
    element
        .get_attribute(NODE_ATTRIBUTE)?
        .parse::<u64>()
        .ok()
        .map(NodeId)
}

/// The ARIA role for an AccessKit role.
///
/// `None` means "do not set a role attribute": a `<div>` with no role is a
/// presentational grouping element, which is the correct projection for a node
/// that only exists to hold children. Setting `role="generic"` instead would
/// add a pointless stop for some screen readers.
fn aria_role(role: Role) -> Option<&'static str> {
    Some(match role {
        Role::Unknown | Role::GenericContainer | Role::Pane | Role::TextRun => return None,

        Role::Button | Role::DefaultButton => "button",
        Role::CheckBox => "checkbox",
        Role::RadioButton => "radio",
        Role::Switch => "switch",
        Role::Link => "link",
        Role::Label | Role::Caption | Role::Legend => "caption",
        Role::Image | Role::SvgRoot => "img",
        Role::Heading => "heading",
        Role::Paragraph | Role::Blockquote => "paragraph",
        Role::Code => "code",
        Role::Emphasis => "emphasis",
        Role::Strong => "strong",
        Role::Mark => "mark",

        Role::TextInput
        | Role::SearchInput
        | Role::EmailInput
        | Role::NumberInput
        | Role::PasswordInput
        | Role::PhoneNumberInput
        | Role::UrlInput
        | Role::DateInput
        | Role::DateTimeInput
        | Role::WeekInput
        | Role::MonthInput
        | Role::TimeInput => "textbox",
        Role::MultilineTextInput => "textbox",
        Role::ComboBox | Role::EditableComboBox => "combobox",
        Role::SpinButton => "spinbutton",
        Role::Slider => "slider",
        Role::ColorWell => "button",

        Role::List | Role::DescriptionList => "list",
        Role::ListItem => "listitem",
        Role::ListMarker => return None,
        Role::ListBox => "listbox",
        Role::ListBoxOption => "option",
        Role::Grid => "grid",
        Role::GridCell => "gridcell",
        Role::Table | Role::LayoutTable => "table",
        Role::Row | Role::LayoutTableRow => "row",
        Role::RowGroup => "rowgroup",
        Role::Cell | Role::LayoutTableCell => "cell",
        Role::RowHeader => "rowheader",
        Role::ColumnHeader => "columnheader",
        Role::Tree => "tree",
        Role::TreeItem => "treeitem",

        Role::Menu | Role::MenuListPopup => "menu",
        Role::MenuBar => "menubar",
        Role::MenuItem | Role::MenuListOption => "menuitem",
        Role::MenuItemCheckBox => "menuitemcheckbox",
        Role::MenuItemRadio => "menuitemradio",
        Role::Tab => "tab",
        Role::TabList => "tablist",
        Role::TabPanel => "tabpanel",
        Role::Toolbar => "toolbar",
        Role::RadioGroup => "radiogroup",
        Role::Group | Role::Section | Role::Details | Role::Figure => "group",

        Role::Dialog => "dialog",
        Role::AlertDialog => "alertdialog",
        Role::Alert => "alert",
        Role::Status => "status",
        Role::Log => "log",
        Role::Marquee => "marquee",
        Role::Timer => "timer",
        Role::Tooltip => "tooltip",
        Role::ProgressIndicator => "progressbar",
        Role::Meter => "meter",
        Role::ScrollBar => "scrollbar",
        Role::Splitter => "separator",
        Role::LineBreak => "separator",
        Role::Search => "search",
        Role::Form => "form",
        Role::Banner | Role::Header => "banner",
        Role::Navigation => "navigation",
        Role::Main => "main",
        Role::Complementary => "complementary",
        Role::ContentInfo | Role::Footer => "contentinfo",
        Role::Region => "region",
        Role::Article => "article",
        Role::Document | Role::RootWebArea => "document",
        Role::Application => "application",
        Role::Note => "note",
        Role::Term => "term",
        Role::Definition => "definition",
        Role::Feed => "feed",
        Role::FigureCaption => "figure",
        Role::Comment => "comment",

        // Everything else is structural or has no ARIA equivalent worth
        // asserting; a wrong role is worse than no role, because assistive
        // technology believes it.
        _ => return None,
    })
}

/// The subset of a node this adapter actually writes to the DOM.
///
/// Held beside the element so a frame that changed nothing compares equal and
/// touches no DOM at all.
#[derive(PartialEq)]
struct Mirrored {
    role: Option<&'static str>,
    label: Option<String>,
    description: Option<String>,
    /// The value, for the handful of roles where ARIA takes one as an
    /// attribute.
    value_text: Option<String>,
    /// The value, for every other role, where it belongs in the element's own
    /// content because that is where a reader looks for it.
    text: Option<String>,
    bounds: Option<Rect>,
    disabled: bool,
    hidden: bool,
    required: bool,
    read_only: bool,
    multiselectable: bool,
    busy: bool,
    selected: Option<bool>,
    toggled: Option<Toggled>,
    level: Option<usize>,
    live: Option<Live>,
    clickable: bool,
    focusable: bool,
    children: Vec<NodeId>,
}

/// The ARIA roles that take `aria-valuetext`.
///
/// Everywhere else -- a textbox above all -- the value is the element's
/// content. Writing it as `aria-valuetext` there is invalid ARIA, and a reader
/// announces nothing for it, so the value would be lost twice over.
fn takes_valuetext(role: Option<&str>) -> bool {
    matches!(
        role,
        Some("slider" | "spinbutton" | "progressbar" | "meter" | "scrollbar")
    )
}

/// An ARIA attribute whose value is the empty string is worse than an absent
/// one: it suppresses the name computation that would otherwise have found the
/// element's content, so the node ends up with no accessible name at all.
fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
}

impl Mirrored {
    fn read(node: &accesskit::Node) -> Self {
        let role = aria_role(node.role());
        let value = non_empty(node.value());
        let children = node.children().to_vec();
        Self {
            role,
            label: non_empty(node.label()),
            description: non_empty(node.description()),
            value_text: takes_valuetext(role).then(|| value.clone()).flatten(),
            // Only a leaf: `set_text_content` replaces every child, and a
            // parent's children are written by the reorder pass, which runs
            // only when the child list changed.
            text: (!takes_valuetext(role) && children.is_empty())
                .then_some(value)
                .flatten(),
            bounds: node.bounds(),
            disabled: node.is_disabled(),
            hidden: node.is_hidden(),
            required: node.is_required(),
            read_only: node.is_read_only(),
            multiselectable: node.is_multiselectable(),
            busy: node.is_busy(),
            selected: node.is_selected(),
            toggled: node.toggled(),
            level: node.level(),
            live: node.live(),
            clickable: node.supports_action(Action::Click),
            focusable: node.supports_action(Action::Focus),
            children,
        }
    }
}

struct MirrorNode {
    element: web_sys::Element,
    applied: Mirrored,
}

/// Projects GPUI's accessibility tree into a hidden DOM subtree.
pub(crate) struct WebA11yAdapter {
    document: web_sys::Document,
    container: web_sys::Element,
    live_region: web_sys::Element,
    input_element: web_sys::HtmlInputElement,
    nodes: HashMap<NodeId, MirrorNode>,
    focus: Option<NodeId>,
    announced: String,
    /// The delegated `click`/`focusin` listeners. Dropping the adapter removes
    /// them, which is why they are owned here rather than forgotten.
    _listeners: Vec<DelegatedListener>,
}

/// A delegated DOM listener that detaches itself when dropped.
struct DelegatedListener {
    target: web_sys::EventTarget,
    kind: &'static str,
    closure: Closure<dyn FnMut(web_sys::Event)>,
}

impl Drop for DelegatedListener {
    fn drop(&mut self) {
        let _ = self
            .target
            .remove_event_listener_with_callback(self.kind, self.closure.as_ref().unchecked_ref());
    }
}

impl WebA11yAdapter {
    /// Build the mirror subtree and wire the two delegated listeners that carry
    /// assistive-technology activation back into GPUI.
    pub(crate) fn new(
        document: web_sys::Document,
        input_element: web_sys::HtmlInputElement,
        action: Rc<dyn Fn(ActionRequest)>,
    ) -> anyhow::Result<Self> {
        let body = document
            .body()
            .ok_or_else(|| anyhow::anyhow!("No `body` found on document"))?;

        let container = document
            .create_element("div")
            .map_err(|error| anyhow::anyhow!("Failed to create a11y container: {error:?}"))?;
        container.set_id("gpui-a11y-root");
        // The mirror covers the canvas so absolute bounds land where the pixels
        // are, and never intercepts pointer input.
        set_style(
            &container,
            &[
                ("position", "fixed"),
                ("inset", "0"),
                ("overflow", "hidden"),
                ("pointer-events", "none"),
                // Transparent rather than `display: none` or `visibility:
                // hidden`, either of which would also hide it from assistive
                // technology, which is the one reader it exists for.
                ("color", "transparent"),
                ("background", "transparent"),
                ("user-select", "none"),
                ("z-index", "0"),
            ],
        );
        body.append_child(&container)
            .map_err(|error| anyhow::anyhow!("Failed to append a11y container: {error:?}"))?;

        let live_region = document
            .create_element("div")
            .map_err(|error| anyhow::anyhow!("Failed to create a11y live region: {error:?}"))?;
        live_region.set_id("gpui-a11y-live");
        let _ = live_region.set_attribute("aria-live", "polite");
        let _ = live_region.set_attribute("aria-atomic", "true");
        set_style(
            &live_region,
            &[
                ("position", "absolute"),
                ("width", "1px"),
                ("height", "1px"),
                ("overflow", "hidden"),
                ("clip", "rect(0 0 0 0)"),
                ("white-space", "nowrap"),
            ],
        );
        container
            .append_child(&live_region)
            .map_err(|error| anyhow::anyhow!("Failed to append a11y live region: {error:?}"))?;

        // One listener per event kind for the whole mirror. A node's id travels
        // on the element, so adding or removing nodes costs no listener work.
        let mut listeners = Vec::new();
        for (kind, requested) in [("click", Action::Click), ("focusin", Action::Focus)] {
            let action = action.clone();
            let closure = Closure::wrap(Box::new(move |event: web_sys::Event| {
                let Some(target) = event.target() else {
                    return;
                };
                let Ok(element) = target.dyn_into::<web_sys::Element>() else {
                    return;
                };
                let Some(node_id) = node_id_from_element(&element) else {
                    return;
                };
                action(ActionRequest {
                    action: requested,
                    target_tree: accesskit::TreeId::ROOT,
                    target_node: node_id,
                    data: None,
                });
            }) as Box<dyn FnMut(web_sys::Event)>);
            container
                .add_event_listener_with_callback(kind, closure.as_ref().unchecked_ref())
                .map_err(|error| anyhow::anyhow!("Failed to listen for {kind}: {error:?}"))?;
            listeners.push(DelegatedListener {
                target: container.clone().into(),
                kind,
                closure,
            });
        }

        Ok(Self {
            document,
            container,
            live_region,
            input_element,
            nodes: HashMap::default(),
            focus: None,
            announced: String::new(),
            _listeners: listeners,
        })
    }

    /// Apply one frame's tree to the mirror.
    ///
    /// `scale_factor` converts AccessKit's logical coordinates into CSS pixels,
    /// which is what the mirror is positioned in.
    pub(crate) fn apply(&mut self, update: &TreeUpdate, scale_factor: f32) {
        let mut announcement: Option<String> = None;

        for (node_id, node) in &update.nodes {
            let read = Mirrored::read(node);

            match self.nodes.get_mut(node_id) {
                Some(existing) => {
                    if existing.applied == read {
                        continue;
                    }
                    // A live node whose name changed is the one thing worth
                    // saying out loud; everything else the reader will reach by
                    // navigating.
                    if matches!(read.live, Some(Live::Polite) | Some(Live::Assertive))
                        && existing.applied.label != read.label
                        && let Some(label) = read.label.clone()
                    {
                        announcement = Some(label);
                    }
                    let reorder = existing.applied.children != read.children;
                    write_node(&existing.element, &read, scale_factor);
                    existing.applied = read;
                    if reorder {
                        self.reorder_children(*node_id);
                    }
                }
                None => {
                    let Ok(element) = self.document.create_element("div") else {
                        continue;
                    };
                    element.set_id(&element_id(*node_id));
                    let _ = element.set_attribute(NODE_ATTRIBUTE, &node_id.0.to_string());
                    write_node(&element, &read, scale_factor);
                    // Parented by the reorder pass below; appending here first
                    // keeps it in the document so `aria-activedescendant` and
                    // `getElementById` resolve even on the frame it appears.
                    let _ = self.container.append_child(&element);
                    self.nodes.insert(
                        *node_id,
                        MirrorNode {
                            element,
                            applied: read,
                        },
                    );
                }
            }
        }

        // Nodes absent from a full tree are gone, not merely unchanged: GPUI
        // rebuilds the whole tree every frame.
        let present: std::collections::HashSet<NodeId> =
            update.nodes.iter().map(|(id, _)| *id).collect();
        self.nodes.retain(|node_id, mirrored| {
            if present.contains(node_id) {
                return true;
            }
            if let Some(parent) = mirrored.element.parent_node() {
                let _ = parent.remove_child(&mirrored.element);
            }
            false
        });

        // Newly created nodes were parked on the container; place every node
        // that has children now that all of them exist.
        for (node_id, node) in &update.nodes {
            if !node.children().is_empty() {
                self.reorder_children(*node_id);
            }
        }

        if self.focus != Some(update.focus) {
            self.focus = Some(update.focus);
            if self.nodes.contains_key(&update.focus) {
                let _ = self
                    .input_element
                    .set_attribute("aria-activedescendant", &element_id(update.focus));
            } else {
                let _ = self.input_element.remove_attribute("aria-activedescendant");
            }
        }

        if let Some(announcement) = announcement
            && announcement != self.announced
        {
            self.live_region.set_text_content(Some(&announcement));
            self.announced = announcement;
        }
    }

    /// Make the DOM children of `node_id` match its AccessKit children, in
    /// order. Only called when that list actually changed.
    fn reorder_children(&self, node_id: NodeId) {
        let Some(parent) = self.nodes.get(&node_id) else {
            return;
        };
        let children = parent.applied.children.clone();
        for child_id in children {
            let Some(child) = self.nodes.get(&child_id) else {
                continue;
            };
            // `append_child` moves an already-parented node, so this both
            // re-parents and orders in one pass.
            let _ = parent.element.append_child(&child.element);
        }
    }
}

impl Drop for WebA11yAdapter {
    fn drop(&mut self) {
        if let Some(parent) = self.container.parent_node() {
            let _ = parent.remove_child(&self.container);
        }
    }
}

fn set_style(element: &web_sys::Element, properties: &[(&str, &str)]) {
    let Some(html) = element.dyn_ref::<web_sys::HtmlElement>() else {
        return;
    };
    let style = html.style();
    for (property, value) in properties {
        let _ = style.set_property(property, value);
    }
}

fn set_or_clear(element: &web_sys::Element, attribute: &str, value: Option<&str>) {
    match value {
        Some(value) => {
            let _ = element.set_attribute(attribute, value);
        }
        None => {
            let _ = element.remove_attribute(attribute);
        }
    }
}

fn set_flag(element: &web_sys::Element, attribute: &str, set: bool) {
    set_or_clear(element, attribute, set.then_some("true"));
}

fn write_node(element: &web_sys::Element, node: &Mirrored, scale_factor: f32) {
    set_or_clear(element, "role", node.role);
    set_or_clear(element, "aria-label", node.label.as_deref());
    set_or_clear(element, "aria-description", node.description.as_deref());
    set_or_clear(element, "aria-valuetext", node.value_text.as_deref());
    // A leaf's value is its content. `Mirrored::read` only fills this in for a
    // node with no children, so this can never replace a mirrored subtree.
    if let Some(text) = node.text.as_deref() {
        element.set_text_content(Some(text));
    } else if node.children.is_empty() {
        element.set_text_content(None);
    }

    set_flag(element, "aria-disabled", node.disabled);
    set_flag(element, "aria-required", node.required);
    set_flag(element, "aria-readonly", node.read_only);
    set_flag(element, "aria-multiselectable", node.multiselectable);
    set_flag(element, "aria-busy", node.busy);
    // `aria-hidden` is the projection of AccessKit's own hidden flag, so a node
    // GPUI excluded from the tree stays excluded here too.
    set_flag(element, "aria-hidden", node.hidden);

    set_or_clear(
        element,
        "aria-selected",
        node.selected
            .map(|selected| if selected { "true" } else { "false" }),
    );
    set_or_clear(
        element,
        "aria-checked",
        node.toggled.map(|toggled| match toggled {
            Toggled::False => "false",
            Toggled::True => "true",
            Toggled::Mixed => "mixed",
        }),
    );
    set_or_clear(
        element,
        "aria-level",
        node.level.map(|level| level.to_string()).as_deref(),
    );
    set_or_clear(
        element,
        "aria-live",
        node.live.and_then(|live| match live {
            Live::Off => None,
            Live::Polite => Some("polite"),
            Live::Assertive => Some("assertive"),
        }),
    );

    // A node the reader can activate must be reachable by the reader's own
    // navigation. `tabindex="-1"` keeps it out of the tab ring -- the hidden
    // input still owns the tab stop -- while making it a legal focus target for
    // an AT-synthesized `focus()`.
    set_or_clear(
        element,
        "tabindex",
        (node.clickable || node.focusable).then_some("-1"),
    );

    match node.bounds {
        Some(bounds) => {
            let scale = f64::from(scale_factor.max(f32::EPSILON));
            set_style(
                element,
                &[
                    ("position", "absolute"),
                    ("left", &format!("{}px", bounds.x0 / scale)),
                    ("top", &format!("{}px", bounds.y0 / scale)),
                    ("width", &format!("{}px", (bounds.x1 - bounds.x0) / scale)),
                    ("height", &format!("{}px", (bounds.y1 - bounds.y0) / scale)),
                    ("pointer-events", "none"),
                    ("margin", "0"),
                ],
            );
        }
        None => {
            // A node with no bounds is structural. It must still exist so the
            // subtree keeps its shape, but it should not claim screen area.
            set_style(
                element,
                &[
                    ("position", "static"),
                    ("width", "0"),
                    ("height", "0"),
                    ("pointer-events", "none"),
                ],
            );
        }
    }
}

/// The retained mirror, exposed so a host can assert that it agrees with
/// `Window::debug_a11y_tree_json` for a fixture frame.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct A11yMirrorSummary {
    /// One entry per mirrored node.
    pub node_count: usize,
    /// The document id currently named by `aria-activedescendant`, if any.
    pub focused_element_id: Option<String>,
}

impl WebA11yAdapter {
    pub(crate) fn summary(&self) -> A11yMirrorSummary {
        A11yMirrorSummary {
            node_count: self.nodes.len(),
            focused_element_id: self.input_element.get_attribute("aria-activedescendant"),
        }
    }
}

thread_local! {
    /// The live adapter for the current window, so a host can read the mirror
    /// summary without owning the window. There is exactly one window on the
    /// web, and exactly one main thread.
    static SUMMARY_SOURCE: RefCell<Option<Rc<dyn Fn() -> A11yMirrorSummary>>> =
        const { RefCell::new(None) };
}

pub(crate) fn publish_summary_source(source: Rc<dyn Fn() -> A11yMirrorSummary>) {
    SUMMARY_SOURCE.with(|cell| *cell.borrow_mut() = Some(source));
}

/// The current accessibility mirror, for oracles that compare it against
/// GPUI's own `debug_a11y_tree_json`.
pub fn a11y_mirror_summary() -> Option<A11yMirrorSummary> {
    SUMMARY_SOURCE.with(|cell| cell.borrow().as_ref().map(|source| source()))
}
