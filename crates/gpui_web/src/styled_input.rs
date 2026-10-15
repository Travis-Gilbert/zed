//! Text/UTF-16 selection adapter for the existing IME conduit. Descendants
//! are presentation only; the focused input handler remains the document.
use std::{cell::RefCell, ops::Range};

use gpui::{FontStyle, HighlightStyle};
use wasm_bindgen::{JsCast, JsValue};

/// One resolved native style over scalar-safe UTF-8 byte offsets.
#[derive(Clone, Debug, PartialEq)]
pub struct StyledTextRun {
    /// Range in the canonical document, in UTF-8 bytes.
    pub range: Range<usize>,
    /// Absolute native presentation after applying the final highlight to the
    /// actual base TextStyle and resolving its selected font face. Color is
    /// final ink (including alpha), not a relative highlight blend.
    pub style: HighlightStyle,
}

/// A style projection that could not be applied without changing authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StyledInputRefusal {
    /// The handle no longer owns the adopted incarnation.
    Stale,
    /// Browser composition owns the descendants until commit or cancellation.
    Busy,
    /// The supplied styles describe another document revision.
    TextMismatch,
    /// A range is reversed, out of bounds, or splits a Unicode scalar.
    InvalidRange,
    /// CSS cannot express the supplied style without dropping a native fact.
    UnsupportedStyle,
    /// The browser refused an operation on the retained subtree.
    Dom,
}
impl From<JsValue> for StyledInputRefusal {
    fn from(_: JsValue) -> Self {
        Self::Dom
    }
}

#[derive(Clone)]
struct Boundary {
    node: web_sys::Node,
    offset: u32,
    text: u32,
}
#[derive(Clone)]
struct Leaf {
    node: web_sys::Text,
    range: Range<u32>,
}
struct Snapshot {
    text: String,
    points: Vec<Boundary>,
    leaves: Vec<Leaf>,
}
impl Snapshot {
    fn new(root: &web_sys::HtmlElement) -> Self {
        let mut result = Self {
            text: String::new(),
            points: vec![],
            leaves: vec![],
        };
        result.visit(root.as_ref(), true);
        result
    }
    fn len(&self) -> u32 {
        self.text.encode_utf16().count() as u32
    }
    fn visit(&mut self, node: &web_sys::Node, root: bool) {
        if let Some(text) = node.dyn_ref::<web_sys::Text>() {
            let start = self.len();
            self.text.push_str(&text.data());
            self.leaves.push(Leaf {
                node: text.clone(),
                range: start..self.len(),
            });
            return;
        }
        let tag = node
            .dyn_ref::<web_sys::Element>()
            .map(|e| e.tag_name())
            .unwrap_or_default();
        if tag == "BR" {
            // Browsers keep one BR in an otherwise empty editing block.
            let placeholder = node.next_sibling().is_none();
            self.points.push(Boundary {
                node: node.clone(),
                offset: 0,
                text: self.len(),
            });
            if !placeholder {
                self.text.push('\n');
            }
            return;
        }
        let block = !root && matches!(tag.as_str(), "DIV" | "P");
        if block && node.previous_sibling().is_some() {
            self.text.push('\n');
        }
        let children = node.child_nodes();
        for i in 0..=children.length() {
            self.points.push(Boundary {
                node: node.clone(),
                offset: i,
                text: self.len(),
            });
            if let Some(child) = children.item(i) {
                self.visit(&child, false);
            }
        }
        if block
            && node.next_sibling().is_some_and(|next| {
                !next
                    .dyn_ref::<web_sys::Element>()
                    .is_some_and(|next| matches!(next.tag_name().as_str(), "DIV" | "P"))
            })
        {
            self.text.push('\n');
        }
    }
    fn offset(&self, node: &web_sys::Node, offset: u32) -> Option<u32> {
        if let Some(leaf) = self.leaves.iter().find(|leaf| <web_sys::Text as AsRef<web_sys::Node>>::as_ref(&leaf.node) == node) {
            return (offset <= leaf.range.end - leaf.range.start)
                .then_some(leaf.range.start + offset);
        }
        self.points
            .iter()
            .find(|point| point.node == *node && point.offset == offset)
            .map(|point| point.text)
    }
    fn point(&self, offset: u32) -> Option<(web_sys::Node, u32)> {
        self.leaves
            .iter()
            .find(|leaf| leaf.range.start <= offset && offset <= leaf.range.end)
            .map(|leaf| (leaf.node.clone().into(), offset - leaf.range.start))
            .or_else(|| {
                self.points
                    .iter()
                    .find(|point| point.text == offset)
                    .map(|point| (point.node.clone(), point.offset))
            })
    }
}

pub(crate) struct StyledInput {
    element: web_sys::HtmlElement,
    applied: RefCell<Option<(String, Vec<StyledTextRun>)>>,
}
impl StyledInput {
    pub(crate) fn new(element: web_sys::HtmlElement) -> Self {
        Self {
            element,
            applied: RefCell::new(None),
        }
    }
    pub(crate) fn element(&self) -> &web_sys::HtmlElement {
        &self.element
    }
    pub(crate) fn invalidate_styles(&self) {
        self.applied.replace(None);
    }
    pub(crate) fn value(&self) -> String {
        Snapshot::new(&self.element).text
    }
    pub(crate) fn selection(&self) -> Option<(u32, u32, bool)> {
        let selection = self.element.owner_document()?.get_selection().ok()??;
        let snapshot = Snapshot::new(&self.element);
        let anchor = snapshot.offset(&selection.anchor_node()?, selection.anchor_offset())?;
        let focus = snapshot.offset(&selection.focus_node()?, selection.focus_offset())?;
        Some((anchor.min(focus), anchor.max(focus), focus < anchor))
    }
    pub(crate) fn set_selection(&self, start: u32, end: u32, reversed: bool) {
        let Some(document) = self.element.owner_document() else {
            return;
        };
        let Ok(Some(selection)) = document.get_selection() else {
            return;
        };
        let snapshot = Snapshot::new(&self.element);
        let Some((start_node, start_offset)) = snapshot.point(start) else {
            return;
        };
        let Some((end_node, end_offset)) = snapshot.point(end) else {
            return;
        };
        let _ = if reversed {
            selection.set_base_and_extent(&end_node, end_offset, &start_node, start_offset)
        } else {
            selection.set_base_and_extent(&start_node, start_offset, &end_node, end_offset)
        };
    }
    pub(crate) fn set_value(&self, value: &str) {
        if let Some(document) = self.element.owner_document() {
            let _ = self.normalize_blocks(&document);
        }
        let snapshot = Snapshot::new(&self.element);
        if snapshot.text == value {
            return;
        }
        let (start, end, inserted) = crate::input_edit::minimal_replacement(&snapshot.text, value);
        let Some(document) = self.element.owner_document() else {
            return;
        };
        let Ok(range) = document.create_range() else {
            return;
        };
        let Some((a, ai)) = snapshot.point(start as u32) else {
            return;
        };
        let Some((b, bi)) = snapshot.point(end as u32) else {
            return;
        };
        if range.set_start(&a, ai).is_err() || range.set_end(&b, bi).is_err() {
            return;
        }
        if range.delete_contents().is_err() {
            return;
        }
        if let Some(text) = a.dyn_ref::<web_sys::Text>() {
            let _ = text.insert_data(ai, &inserted);
        } else {
            let _ = range.insert_node(&document.create_text_node(&inserted));
        }
        self.applied.replace(None);
    }
    fn normalize_blocks(&self, document: &web_sys::Document) -> Result<(), StyledInputRefusal> {
        if self.element.query_selector("div,p,br")?.is_none() {
            return Ok(());
        }
        let snapshot = Snapshot::new(&self.element);
        let units: Vec<u16> = snapshot.text.encode_utf16().collect();
        let mut desired: Vec<web_sys::Node> = Vec::new();
        let mut offset = 0u32;
        for leaf in snapshot.leaves {
            if offset < leaf.range.start {
                desired.push(
                    document
                        .create_text_node(&String::from_utf16_lossy(
                            &units[offset as usize..leaf.range.start as usize],
                        ))
                        .into(),
                );
            }
            offset = leaf.range.end;
            desired.push(leaf.node.into());
        }
        if offset < units.len() as u32 {
            desired.push(
                document
                    .create_text_node(&String::from_utf16_lossy(&units[offset as usize..]))
                    .into(),
            );
        }
        // Move original Text nodes out of browser-created blocks before
        // retiring those blocks; never replace surviving text nodes.
        for node in &desired {
            self.element.append_child(node)?;
        }
        let children = self.element.child_nodes();
        let retired = (0..children.length())
            .filter_map(|i| children.item(i))
            .filter(|node| !desired.contains(node))
            .collect::<Vec<_>>();
        for node in retired {
            self.element.remove_child(&node)?;
        }
        Ok(())
    }
    pub(crate) fn set_highlights(
        &self,
        expected: &str,
        runs: Vec<StyledTextRun>,
    ) -> Result<(), StyledInputRefusal> {
        if self.value() != expected {
            return Err(StyledInputRefusal::TextMismatch);
        }
        for run in &runs {
            if run.range.start > run.range.end
                || run.range.end > expected.len()
                || !expected.is_char_boundary(run.range.start)
                || !expected.is_char_boundary(run.range.end)
            {
                return Err(StyledInputRefusal::InvalidRange);
            }
            validate_style(&run.style)?;
        }
        if self
            .applied
            .borrow()
            .as_ref()
            .is_some_and(|(text, old)| text == expected && *old == runs)
        {
            return Ok(());
        }
        let selection = self.selection();
        let document = self
            .element
            .owner_document()
            .ok_or(StyledInputRefusal::Dom)?;
        let mut boundaries = vec![0, expected.len()];
        for run in &runs {
            boundaries.extend([run.range.start, run.range.end]);
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        let utf16: Vec<u32> = boundaries
            .iter()
            .map(|&byte| expected[..byte].encode_utf16().count() as u32)
            .collect();
        // Validate every resulting interval before touching the retained tree.
        for interval in boundaries.windows(2) {
            let mut style = HighlightStyle::default();
            for run in &runs {
                if run.range.start <= interval[0] && interval[0] < run.range.end {
                    merge_style(&mut style, &run.style);
                }
            }
            validate_style(&style)?;
        }
        self.normalize_blocks(&document)?;
        // Retire only our prior wrappers, preserving every browser text node.
        let wrappers = self.element.query_selector_all("[data-gpui-text-run]")?;
        for i in (0..wrappers.length()).rev() {
            let Some(wrapper) = wrappers.item(i) else {
                continue;
            };
            let Some(parent) = wrapper.parent_node() else {
                continue;
            };
            while let Some(child) = wrapper.first_child() {
                parent.insert_before(&child, Some(&wrapper))?;
            }
            parent.remove_child(&wrapper)?;
        }
        // Splitting and reparenting retains the browser's original Text nodes.
        // Replacing all descendants destroys Firefox's native history trigger.
        for &boundary in utf16.iter().rev() {
            let snapshot = Snapshot::new(&self.element);
            if let Some(leaf) = snapshot
                .leaves
                .iter()
                .find(|leaf| leaf.range.start < boundary && boundary < leaf.range.end)
            {
                leaf.node.split_text(boundary - leaf.range.start)?;
            }
        }
        let snapshot = Snapshot::new(&self.element);
        for leaf in snapshot.leaves {
            let byte = expected
                .char_indices()
                .scan(0u32, |offset, (byte, ch)| {
                    let old = *offset;
                    *offset += ch.len_utf16() as u32;
                    Some((old, byte))
                })
                .find(|(offset, _)| *offset == leaf.range.start)
                .map(|(_, byte)| byte)
                .unwrap_or(expected.len());
            let mut style = HighlightStyle::default();
            for run in &runs {
                if run.range.start <= byte && byte < run.range.end {
                    merge_style(&mut style, &run.style);
                }
            }
            let span: web_sys::HtmlElement = document
                .create_element("span")?
                .dyn_into()
                .map_err(|_| StyledInputRefusal::Dom)?;
            span.set_attribute("data-gpui-text-run", "")?;
            apply_style(&span, &style)?;
            let parent = leaf.node.parent_node().ok_or(StyledInputRefusal::Dom)?;
            // Remove the previous presentation wrapper, retaining its text child.
            if parent
                .dyn_ref::<web_sys::Element>()
                .is_some_and(|parent| parent.has_attribute("data-gpui-text-run"))
                && parent.child_nodes().length() == 1
            {
                let grand = parent.parent_node().ok_or(StyledInputRefusal::Dom)?;
                grand.insert_before(&span, Some(&parent))?;
                span.append_child(&leaf.node)?;
                grand.remove_child(&parent)?;
            } else {
                parent.insert_before(&span, Some(&leaf.node))?;
                span.append_child(&leaf.node)?;
            }
        }
        if let Some((start, end, reversed)) = selection {
            self.set_selection(start, end, reversed);
        }
        self.applied.replace(Some((expected.to_owned(), runs)));
        Ok(())
    }
}
fn merge_style(target: &mut HighlightStyle, source: &HighlightStyle) {
    if source.color.is_some() {
        target.color = source.color;
    }
    if source.background_color.is_some() {
        target.background_color = source.background_color;
    }
    if source.font_weight.is_some() {
        target.font_weight = source.font_weight;
    }
    if source.font_style.is_some() {
        target.font_style = source.font_style;
    }
    if source.underline.is_some() {
        target.underline = source.underline;
    }
    if source.strikethrough.is_some() {
        target.strikethrough = source.strikethrough;
    }
    if source.fade_out.is_some() {
        target.fade_out = source.fade_out;
    }
}
fn validate_style(style: &HighlightStyle) -> Result<(), StyledInputRefusal> {
    // CSS shares decoration thickness/color for underline and strike; native
    // styles may differ. Refuse that combination rather than silently lose it.
    if let (Some(a), Some(b)) = (style.underline, style.strikethrough) {
        if a.thickness != b.thickness || a.color != b.color || a.wavy {
            return Err(StyledInputRefusal::UnsupportedStyle);
        }
    }
    if style.fade_out.is_some_and(|fade| fade != 0.0) {
        return Err(StyledInputRefusal::UnsupportedStyle);
    }
    Ok(())
}
fn apply_style(element: &web_sys::HtmlElement, style: &HighlightStyle) -> Result<(), JsValue> {
    let css = element.style();
    if let Some(color) = style.color {
        css.set_property("color", &color.to_string())?;
    }
    if let Some(color) = style.background_color {
        css.set_property("background-color", &color.to_string())?;
    }
    if let Some(weight) = style.font_weight {
        css.set_property("font-weight", &weight.0.to_string())?;
    }
    if let Some(font) = style.font_style {
        css.set_property(
            "font-style",
            match font {
                FontStyle::Normal => "normal",
                FontStyle::Italic => "italic",
                FontStyle::Oblique => "oblique",
            },
        )?;
    }
    let mut lines = vec![];
    if style.underline.is_some() {
        lines.push("underline");
    }
    if style.strikethrough.is_some() {
        lines.push("line-through");
    }
    if !lines.is_empty() {
        css.set_property("text-decoration-line", &lines.join(" "))?;
    }
    if let Some(underline) = style.underline {
        css.set_property(
            "text-decoration-thickness",
            &format!("{}px", f32::from(underline.thickness)),
        )?;
        css.set_property(
            "text-decoration-style",
            if underline.wavy { "wavy" } else { "solid" },
        )?;
        if let Some(color) = underline.color {
            css.set_property("text-decoration-color", &color.to_string())?;
        }
    } else if let Some(strike) = style.strikethrough {
        css.set_property(
            "text-decoration-thickness",
            &format!("{}px", f32::from(strike.thickness)),
        )?;
        if let Some(color) = strike.color {
            css.set_property("text-decoration-color", &color.to_string())?;
        }
    }
    Ok(())
}
