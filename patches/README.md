# Theorem Zed patch series

The pin catch-up patch records browser prerequisites already present at
`6d4d90754f7dde3e62afb6fe74a632c0b4660396` but absent from the older series.
It changes no production source at the consumer pin. Replay now retains those
exact pre-existing platform, single-line input and browser editing bytes before
applying the native-motion extension.

The native reduced-motion patch feeds macOS's NSWorkspace preference into
GPUI's existing App policy. It owns its notification token and foreground task,
coalesces wake signals through a bounded channel, and fences delivery after
observer replacement. Unsupported platforms preserve the existing policy.
`cargo nextest run --locked -p gpui --lib --features gpui_platform/runtime_shaders`
checks the GPUI components. `cargo run --locked -p gpui_macos --example
native_motion --features runtime_shaders` checks the actual macOS adapter;
`GPUI_NATIVE_MOTION_WAIT_FOR_CHANGE=1` additionally waits for a manual OS toggle.
Posted notification tests are not evidence of an actual System Settings change.

This hard-fork series is applied in the order recorded by `series`. It is not
an upstream contribution queue.

The base used to generate this series is Zed commit
`5b055fa789a8b8d38ac951a6e0cde272f66b4495`. The first patch carries the
external WGPU surface work originally recorded as Theorem commit
`d9849af9f00e33e6f466f514c34e2fc37761d2f8`; the remaining patches carry the
Theorem web accessibility and IME seams that are not present at that base.
The final patch keeps Linux keyring support on the API-equivalent
`oo7 0.6.0-alpha` release, whose declared MSRV is Rust 1.86; the stable 0.6.0
release raised its MSRV to Rust 1.92 after the GPUI Kit 0.6 contract fixed the
consumer toolchain at Rust 1.90. The following patch expresses the same MSRV
boundary in `gpui_util` without the newer `slice::as_array` helper.
The last MSRV patch retains the cold-branch optimization and Unicode-safe text
truncation without the newer `std::hint::cold_path` and
`str::ceil_char_boundary` helpers. The final MSRV patch applies the same cold
branch compatibility to action profiling.

The IME integration patch reconciles the retained accessibility and caret seams
with the upstream textarea-owning `ImeMirror`. It removes the duplicate legacy
focus callback and preserves configuration, composition and text ownership.

The final lock correction restores `gpui_windows` to the Windows Core 0.62
family required by its manifest. The earlier keyring patch accidentally changed
that unrelated edge to 0.57, preventing locked workspace resolution.

Regenerate the files from the rebased commits with `git format-patch`, preserve
their order in `series`, and run the patch-series workflow before moving a
consumer pin. If upstream makes a patch redundant, remove it from the series
instead of retaining an empty compatibility patch.

Pull requests changing the series, its workflow, or GPUI Web verify the recorded
series. Manual workflow runs also verify the recorded series by default. Enable
`rebase_upstream` to also publish an upstream-rebase maintenance branch and PR;
scheduled runs retain that maintenance behavior automatically.

The ASHPD patch selects 0.13.6, the first 0.13 patch declaring Rust 1.87
compatibility. It retains the dependency graph and GPUI portal APIs from 0.13.2,
whose Rust 1.92 minimum prevented the required Rust 1.90 Linux check.

The desktop focus patch keeps the read-only IME mirror focused when an editor
closes, because it also receives GPUI keyboard shortcuts. Coarse-pointer
environments retain the existing blur path for software-keyboard dismissal;
the same pointer-capability query serves both policies. TheoremWeb browser
interaction and direct DOM focus checks validate this prerequisite separately
from the source/locked Linux compiler gate.

The selected-text replacement patch prevents common-prefix/suffix trimming from
removing text inside the mirror's pre-edit selection. It retains the existing
post-edit-caret disambiguation and resolves editor coordinates through fresh
selection anchors. The same helper protects UTF-16 surrogate boundaries. Its
five production-helper tests cover selection replacement, repeated text,
autocorrection, emoji, and exhaustive bounded edits after a remote anchor shift.
The browser Wikia edit oracle exposed the original failure; its real DOM and
canonical document retry remains a separate consumer acceptance gate.

The platform native element seam lets an editable leaf ask whether the platform
is already drawing the control. It is one question rather than a
create/place/destroy protocol, because placement already flows through the
existing caret bounds and a second geometry channel would be a second authority
for one rectangle. Every platform answers `None` by default, so the seam is
inert until a platform overrides it.

The native editable leaf patch draws browser-realized elements on a second
transparent canvas stacked over the wgpu canvas, because wgpu holds that
canvas's context and the proposal's draw calls need one of their own. Support
requires all four entry points -- `requestPaint`, `drawElementImage`,
`updateElementGeometry` and a reflecting `layoutsubtree` -- because an element
that is laid out and never drawn is an invisible control. The accessibility
mirror suppresses a natively realized node so a screen reader meets the control
once. `window.__gpui_html_in_canvas` forces either path for tests; forcing the
native path on a browser that cannot draw the element is refused, not honored.
On every browser shipping today the feature is absent, the layer is never
built, and the cost is four prototype lookups at window creation.

The element layer surface patch sizes the layer canvas to the renderer's
physical size, because a canvas left at its 300x150 default draws into a
thumbnail whatever its CSS size says, and scales the paint by the device pixel
ratio so an element's CSS box lands where it was laid out. It also sets
`pointer-events: auto` on each adopted element, since the layer's own
`pointer-events: none` inherits and would otherwise make the drawn control
unclickable.

The shipping-surface patch corrects what the detection asks for. The prose's
`updateElementGeometry` is defined by no implementation, and because the
support gate required all four probes, the native path could never be reached
-- the fallback was permanent and looked like feature detection. Chrome
151.0.7922.34 with `--enable-blink-features=CanvasDrawElement` ships
`requestPaint`, `layoutSubtree`, `captureElementImage` and
`getElementTransform` on `HTMLCanvasElement` and `drawElementImage` on the 2D
context, and ships neither `updateElementGeometry` nor `placeElement`. The
fourth probe is now `getElementTransform`, which is the geometry seam that
replaced the prose's call and reports a drawn element's transform to the page
rather than taking a rectangle from it.

Input does not follow the draw: a click inside a drawn copy at (90, 72) reaches
`body`, while a click on the same element's layout box at (60, 12) focuses it
and accepts typing. A browser hit-tests, focuses, routes IME to and reports to
assistive technology the element's CSS box, and the canvas draw is a picture.
Drawing at the element's own border box is therefore the only position under
which the pixels and the hit region are one rectangle. The draw also refuses
outside a `paint` event, so it stays in the paint listener where it already
was.

The same patch stops the native path painting a white box over the scene. A
textarea contributes an opaque background, a border, a focus ring and a second
copy of the text in the user agent's font; section 11 grants the browser caret,
selection, spellcheck and autocorrect and nothing more. `color: transparent`
removes the duplicate glyphs and keeps the selection highlight, which paints
behind text, and the caret, which has its own color property.

The `aria_disabled` patch adds the one AccessKit state GPUI could not publish:
`StatefulInteractiveElement::aria_disabled(bool)` writes the node's `disabled`
flag and nothing else. It does not block input or change drawing, so a
component that disables itself keeps owning both and the reader hears the
state. `false` publishes nothing.

The IME entry patch names the element that holds keyboard focus for canvas
text entry. Every conduit is built with `aria-label="Text input"`; while a text
input is focused, the accessibility adapter replaces that with the input's own
label (and sets `aria-description` from its description), on the painted path
as well as the browser-native one, and puts the fixed name back when focus
leaves every text field. Names come from labels only: the node's value is never
read. A blank label is treated as absent, because an empty `aria-label`
suppresses name computation instead of supplying one. The same patch collects
the availability attributes the mirror writes (`aria-disabled`,
`aria-description`, `aria-required`, `aria-readonly`) into one pure table,
`state_attributes`, which the unit tests exercise without a DOM; the
projection itself was already on the mirror element, never a synthetic child.

The editing island patch lets a page hand the window a visible textarea it
created. While live, the island is the IME conduit: it shares the hidden
conduit's listeners, so every edit is imported through the one path, and the
mirror writes the focused input handler's whole document back as the smallest
replacement (`minimal_replacement`, which never cuts a surrogate pair; unit
tested). Focused writes go through `execCommand` so the browser keeps offering
Undo, and browser history intents are routed to the document's one history
through the keybinding path rather than imported. Selection direction is
carried both ways through the new `set_text_selection`. The page keeps the
element's placement and its accessible name.

The platform-element drag and web tab-boundary patch lets a DOM-owned source
start a typed drag through `App::start_drag`, which installs the same `AnyDrag`
consumed by GPUI's existing drag-over and drop handlers. It refuses replacement
of an active or platform-owned drag. Existing `stop_active_drag` cancels it;
the platform-element test proves typed movement, exactly one drop and cancellation.
No second drag controller or domain operation is introduced.

Browser windows traverse tab stops without wrapping, clear GPUI focus at an
edge and propagate the Tab action. `gpui_web` then blurs its input conduit and
leaves the browser's default Tab traversal enabled. Desktop tab traversal stays
cyclic. The non-wrapping map test covers both edges, disabled stops and the
retained desktop cycle; the consumer browser oracle proves actual DOM handoff.

The composing island replacement patch refuses `EditingIsland::adopt` before
changing the current conduit or attaching listeners while an IME composition
is open. A retained editor therefore remains authoritative after an attempted
replacement. The consumer editing oracle attempts a replacement during a
composition and checks refusal, the predecessor's live state and DOM identity,
and the subsequent composition commit through the original listener path.

## Styled input and explicit focus intent (patch 0044)

`StyledEditingIsland` adopts a page-owned contenteditable root through the same
conduit listeners, import, history and composition path as `EditingIsland`. The
conduit owns literal text, UTF-16 directional selection and presentation spans.
Resolved native styles are expected-text gated; composition and retired adoption
incarnations refuse style writes. Restyling retains original browser Text nodes
so Firefox continues emitting its native history intent. No user HTML is parsed.

Explicit InputState focus requests can restore platform focus even when the
native handle is already focused. Requests wait for the current input handler's
paint and are cancelled by a changed focus owner. Native platform implementations
retain the default no-op. Read-only selected-face metadata and an opt-in test
context text-system constructor support actual font/caret qualification.

Native focus intent and selected-face behavior tests pass. The same Rust DOM
adapter passes 14 assertions on each of Chromium, WebKit and Firefox. Those
scoped proofs do not replace the consumer's full retained editor, OS IME,
composition/history, glyph geometry, or deployed product acceptance gates.

## Staged native presentation (patch 0045)

A cold visible root can prepare validated presentation metadata before canonical
text has reached it. The existing mirror applies that metadata only after its
own current, live conduit reconciles exactly the expected text and selection.
Preparation never writes text or moves selection; the strict setter still
refuses mismatched text. Active composition and retired adoption incarnations
refuse preparation, and identical already-applied metadata remains ready.

## Platform composition fact (patch 0046)

`Window::is_text_composing` reads the platform's current composition phase.
The web implementation returns its existing current-conduit flag, including
an empty hidden-input composition before marked text exists. Other platforms
retain the default false unless they report that fact. This observation never
imports text, changes focus, or creates a second composition state. A native
forwarding/phase-transition test and the web library wasm check qualify the API;
consumer refusal tests remain separate.
