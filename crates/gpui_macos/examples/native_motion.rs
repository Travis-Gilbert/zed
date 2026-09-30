//! Real macOS preference smoke test. Does not change system settings.
//!
//! Startup proof:
//! `cargo run --locked -p gpui_macos --example native_motion`
//! Live-change proof (toggle System Settings > Accessibility > Display > Reduce motion):
//! `GPUI_NATIVE_MOTION_WAIT_FOR_CHANGE=1 cargo run --locked -p gpui_macos --example native_motion`

#[cfg(target_os = "macos")]
fn main() {
    use gpui::Platform as _;
    use std::{
        cell::Cell,
        rc::Rc,
        time::{Duration, Instant},
    };

    let platform = Rc::new(gpui_macos::MacPlatform::new(false));
    let initial = platform
        .reduce_motion()
        .expect("macOS preference available");
    let wait_for_change = std::env::var_os("GPUI_NATIVE_MOTION_WAIT_FOR_CHANGE").is_some();
    gpui::Application::with_platform(platform.clone()).run(move |cx| {
        assert_eq!(
            cx.reduce_motion(),
            initial,
            "startup preference synchronization"
        );
        println!("native-motion: startup matched OS ({initial})");
        // Post the real native notification while App is already borrowed.
        // This tests adapter delivery, not an OS setting change.
        cx.set_reduce_motion(!initial);
        for _ in 0..1_000 {
            post_display_notification();
        }
        assert_eq!(cx.reduce_motion(), !initial, "native callback must not re-enter App");

        let canceled_calls = Rc::new(Cell::new(0));
        let dropped_platform = gpui_macos::MacPlatform::new(true);
        dropped_platform.on_reduce_motion_change(Box::new({
            let calls = canceled_calls.clone();
            move |_| calls.set(calls.get() + 1)
        }));
        post_display_notification();
        drop(dropped_platform);

        let replaced_calls = Rc::new(Cell::new(0));
        let replacement_calls = Rc::new(Cell::new(0));
        let replacement_platform = gpui_macos::MacPlatform::new(true);
        replacement_platform.on_reduce_motion_change(Box::new({
            let calls = replaced_calls.clone();
            move |_| calls.set(calls.get() + 1)
        }));
        post_display_notification();
        replacement_platform.on_reduce_motion_change(Box::new({
            let calls = replacement_calls.clone();
            move |_| calls.set(calls.get() + 1)
        }));
        post_display_notification();
        let self_replaced_calls = Rc::new(Cell::new(0));
        let self_replacement_calls = Rc::new(Cell::new(0));
        let self_replacement_platform = Rc::new(gpui_macos::MacPlatform::new(true));
        self_replacement_platform.on_reduce_motion_change(Box::new({
            let platform = Rc::downgrade(&self_replacement_platform);
            let old_calls = self_replaced_calls.clone();
            let new_calls = self_replacement_calls.clone();
            move |_| {
                old_calls.set(old_calls.get() + 1);
                let platform = platform.upgrade().expect("smoke retains platform");
                platform.on_reduce_motion_change(Box::new({
                    let calls = new_calls.clone();
                    move |_| calls.set(calls.get() + 1)
                }));
            }
        }));
        for _ in 0..1_000 {
            post_display_notification();
        }
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_millis(100)).await;
            assert_eq!(cx.update(|cx| cx.reduce_motion()), platform.reduce_motion().unwrap());
            assert_eq!(canceled_calls.get(), 0, "dropped platform cancels pending delivery");
            assert_eq!(replaced_calls.get(), 0, "replacement cancels old delivery");
            assert!(replacement_calls.get() > 0, "replacement receives notification");
            assert!(replacement_calls.get() <= 2, "no-poll burst has bounded delivery");
            assert_eq!(self_replaced_calls.get(), 1, "self-replacement fences buffered old wakes");
            post_display_notification();
            cx.background_executor().timer(Duration::from_millis(100)).await;
            assert!(self_replacement_calls.get() > 0, "self-replacement receives new wake");
            drop(replacement_platform);
            drop(self_replacement_platform);
            println!("native-motion: native notification deferral, burst coalescing, replacement and teardown passed");
            if !wait_for_change {
                cx.update(|cx| cx.quit());
                return;
            }
            println!("native-motion: toggle Reduce motion in System Settings within 60 seconds");
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let native = platform
                    .reduce_motion()
                    .expect("macOS preference available");
                let adopted = cx.update(|cx| cx.reduce_motion());
                if native != initial && adopted == native {
                    println!("native-motion: live OS change adopted ({native})");
                    cx.update(|cx| cx.quit());
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "no live OS change adopted before deadline"
                );
            }
        })
        .detach();
    });
}

#[cfg(target_os = "macos")]
fn post_display_notification() {
    use cocoa::base::{id, nil};
    use objc::{class, msg_send, sel, sel_impl};
    // SAFETY: this smoke runs on AppKit's main thread. The notification is
    // intentionally posted to NSWorkspace's center without changing settings.
    unsafe {
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let center: id = msg_send![workspace, notificationCenter];
        let name = cocoa::foundation::NSString::alloc(nil);
        let name = cocoa::foundation::NSString::init_str(
            name,
            "NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification",
        );
        let _: () = msg_send![center, postNotificationName: name object: workspace];
        let _: () = msg_send![name, release];
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("native-motion: macOS-only OS preference smoke test");
    std::process::exit(2);
}
