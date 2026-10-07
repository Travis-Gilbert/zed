//! Public Window integration checks for decorative retained capture isolation.
//! These exercise TestPlatform input/scope behavior, not native texture publication.
use gpui::prelude::*;
use gpui::{
    App, Bounds, Context, DispatchPhase, FocusHandle, InputHandler, IntoElement, KeyDownEvent,
    Modifiers, MouseDownEvent, Render, RetainedCaptureConfig, Styled, TestAppContext,
    UTF16Selection, Window, canvas, div, point, px, size,
};
use std::{
    cell::Cell,
    ops::Range,
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
};

#[test]
fn nested_and_panicking_capture_restore_window_geometry_and_focus() {
    let mut app = TestAppContext::single();
    let visual = app.add_empty_window();
    let (trusted, captured) = visual.update(|window, cx| {
        let trusted = cx.focus_handle();
        let captured = cx.focus_handle();
        trusted.focus(window, cx);
        (trusted, captured)
    });
    visual.draw(
        point(px(31.), px(47.)),
        size(px(600.), px(400.)),
        move |_, _| {
            canvas(
                move |_, window, cx| {
                    let before = (
                        window.viewport_size(),
                        window.scale_factor(),
                        window.content_mask().bounds,
                        window.element_offset(),
                        window.is_a11y_active(),
                    );
                    let first =
                        RetainedCaptureConfig::new(size(px(320.), px(180.)), 2., 2048).unwrap();
                    let nested =
                        RetainedCaptureConfig::new(size(px(80.), px(60.)), 1.5, 2048).unwrap();
                    window.with_retained_capture(first, |window| {
                        assert!(window.is_retained_capture());
                        assert_eq!(window.viewport_size(), first.logical_size());
                        assert_eq!(window.scale_factor(), 2.);
                        assert_eq!(
                            window.content_mask().bounds,
                            Bounds::new(point(px(0.), px(0.)), first.logical_size())
                        );
                        assert_eq!(window.element_offset(), point(px(0.), px(0.)));
                        assert!(!window.is_a11y_active());
                        captured.focus(window, cx);
                        assert!(trusted.is_focused(window));
                        let refused = catch_unwind(AssertUnwindSafe(|| {
                            window.with_retained_capture(nested, |window| {
                                assert_eq!(window.viewport_size(), nested.logical_size());
                                assert_eq!(window.scale_factor(), 1.5);
                                assert!(window.is_retained_capture());
                                panic!("nested capture fixture");
                            });
                        }));
                        assert!(refused.is_err());
                        assert!(window.is_retained_capture());
                        assert_eq!(window.viewport_size(), first.logical_size());
                        assert_eq!(window.scale_factor(), first.scale_factor());
                    });
                    assert!(!window.is_retained_capture());
                    assert_eq!(
                        (
                            window.viewport_size(),
                            window.scale_factor(),
                            window.content_mask().bounds,
                            window.element_offset(),
                            window.is_a11y_active()
                        ),
                        before
                    );
                    let refused = catch_unwind(AssertUnwindSafe(|| {
                        window.with_retained_capture(first, |_| panic!("outer capture fixture"));
                    }));
                    assert!(refused.is_err());
                    assert!(!window.is_retained_capture());
                    assert_eq!(
                        (
                            window.viewport_size(),
                            window.scale_factor(),
                            window.content_mask().bounds,
                            window.element_offset(),
                            window.is_a11y_active()
                        ),
                        before
                    );
                    captured.focus(window, cx);
                    assert!(
                        captured.is_focused(window),
                        "trusted focus must work after unwinding"
                    );
                },
                |_, (), _, _| {},
            )
            .size_full()
        },
    );
}

#[derive(Clone, Default)]
struct Counts {
    captured_mouse: Rc<Cell<usize>>,
    trusted_mouse: Rc<Cell<usize>>,
    captured_key: Rc<Cell<usize>>,
    trusted_key: Rc<Cell<usize>>,
    captured_text: Rc<Cell<usize>>,
    trusted_text: Rc<Cell<usize>>,
}
struct InputRoot {
    focus: FocusHandle,
    counts: Counts,
}
impl Render for InputRoot {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let counts = self.counts.clone();
        let focus = self.focus.clone();
        let prepaint_focus = self.focus.clone();
        div().size_full().track_focus(&self.focus).child(
            canvas(
                move |_, window, cx| window.set_focus_handle(&prepaint_focus, cx),
                move |_, (), window, cx| {
                    // Register trusted text first: a leaked capture handler would replace it.
                    window.handle_input(&focus, TextSink(counts.trusted_text.clone()), cx);
                    let config =
                        RetainedCaptureConfig::new(size(px(100.), px(100.)), 1., 2048).unwrap();
                    window.with_retained_capture(config, |window| {
                        window.handle_input(&focus, TextSink(counts.captured_text.clone()), cx);
                        let mouse = counts.captured_mouse.clone();
                        window.on_mouse_event(move |_: &MouseDownEvent, phase, _, _| {
                            if phase == DispatchPhase::Bubble {
                                mouse.set(mouse.get() + 1);
                            }
                        });
                        let key = counts.captured_key.clone();
                        window.on_key_event(move |_: &KeyDownEvent, phase, _, _| {
                            if phase == DispatchPhase::Bubble {
                                key.set(key.get() + 1);
                            }
                        });
                    });
                    let mouse = counts.trusted_mouse.clone();
                    window.on_mouse_event(move |_: &MouseDownEvent, phase, _, _| {
                        if phase == DispatchPhase::Bubble {
                            mouse.set(mouse.get() + 1);
                        }
                    });
                    let key = counts.trusted_key.clone();
                    window.on_key_event(move |_: &KeyDownEvent, phase, _, _| {
                        if phase == DispatchPhase::Bubble {
                            key.set(key.get() + 1);
                        }
                    });
                },
            )
            .size_full(),
        )
    }
}

#[test]
fn captured_mouse_and_keyboard_listeners_do_not_steal_trusted_input() {
    let mut app = TestAppContext::single();
    let counts = Counts::default();
    let (root, visual) = app.add_window_view(|window, cx| {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        InputRoot {
            focus,
            counts: counts.clone(),
        }
    });
    visual.simulate_click(point(px(20.), px(20.)), Modifiers::default());
    // Pointer dispatch can blur a text endpoint; establish the keyboard owner
    // explicitly before challenging the captured registration.
    visual.update(|window, cx| {
        let focus = root.read(cx).focus.clone();
        focus.focus(window, cx);
        assert!(focus.is_focused(window));
    });
    visual.run_until_parked();
    visual.simulate_keystrokes("a");
    visual.simulate_input("z");
    assert_eq!(counts.captured_mouse.get(), 0);
    assert_eq!(counts.captured_key.get(), 0);
    assert_eq!(counts.captured_text.get(), 0);
    assert!(
        counts.trusted_text.get() > 0,
        "outside text handler must receive typing"
    );
    assert!(
        counts.trusted_mouse.get() > 0,
        "outside mouse listener must receive physical input"
    );
    assert!(
        counts.trusted_key.get() > 0,
        "outside key listener must receive physical input"
    );
}

// Minimal real platform text-input endpoint. Counting replacements detects a
// captured handler replacing the trusted endpoint; no document model is mocked.
struct TextSink(Rc<Cell<usize>>);
impl InputHandler for TextSink {
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut App,
    ) -> Option<UTF16Selection> {
        None
    }
    fn marked_text_range(&mut self, _: &mut Window, _: &mut App) -> Option<Range<usize>> {
        None
    }
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<String> {
        None
    }
    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        _: &str,
        _: &mut Window,
        _: &mut App,
    ) {
        self.0.set(self.0.get() + 1);
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        _: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        _: &mut App,
    ) {
        self.0.set(self.0.get() + 1);
    }
    fn unmark_text(&mut self, _: &mut Window, _: &mut App) {}
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<Bounds<gpui::Pixels>> {
        None
    }
    fn character_index_for_point(
        &mut self,
        _: gpui::Point<gpui::Pixels>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<usize> {
        None
    }
}
