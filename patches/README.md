# Theorem Zed patch series

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
