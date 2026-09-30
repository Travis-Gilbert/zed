//! Native preference input; GPUI's App remains the motion-policy owner.

use block::ConcreteBlock;
use cocoa::base::{BOOL, YES, id, nil};
use futures::{StreamExt as _, channel::mpsc};
use gpui::{ForegroundExecutor, Task};
use objc::{class, msg_send, sel, sel_impl};
use parking_lot::Mutex;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(crate) fn current() -> bool {
    // SAFETY: called on GPUI's main thread. NSWorkspace and this getter are
    // available before the platform's macOS 10.15.7 deployment target.
    unsafe {
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let reduced: BOOL = msg_send![workspace, accessibilityDisplayShouldReduceMotion];
        reduced == YES
    }
}

/// Owns both native registration and foreground delivery, without a pointer
/// back to MacPlatform or a strong App reference.
pub(crate) struct ReducedMotionObserver {
    center: id,
    token: id,
    active: Arc<AtomicBool>,
    _delivery: Task<()>,
}

impl ReducedMotionObserver {
    pub(crate) fn new(executor: &ForegroundExecutor, mut callback: Box<dyn FnMut(bool)>) -> Self {
        // Coalesce wake signals, not preference values: delivery reads the
        // latest OS preference, so a full queue cannot lose the final value.
        let (sender, mut receiver) = mpsc::channel(1);
        let sender = Mutex::new(sender);
        let block = ConcreteBlock::new(move |_: id| {
            // Cocoa may invoke/copy the block on another thread. Capture only
            // this sendable channel, never an Rc or a GPUI App handle.
            let _ = sender.lock().try_send(());
        })
        .copy();
        let (center, token) = unsafe {
            // SAFETY: registration is on the main thread. Apple's display
            // accessibility notification belongs to NSWorkspace's center,
            // not NSNotificationCenter.defaultCenter. The center copies the
            // block; queue:nil permits arbitrary posting threads, which only
            // enqueue a sendable signal. Own retains for teardown below.
            let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
            let center: id = msg_send![workspace, notificationCenter];
            let name =
                crate::ns_string("NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification");
            let token: id = msg_send![center,
                addObserverForName: name
                object: workspace
                queue: nil
                usingBlock: &*block
            ];
            let _: id = msg_send![center, retain];
            let _: id = msg_send![token, retain];
            (center, token)
        };
        let active = Arc::new(AtomicBool::new(true));
        let delivery_active = active.clone();
        let delivery = executor.spawn(async move {
            while receiver.next().await.is_some() {
                // Cancellation cannot interrupt a callback's current poll.
                // Fence buffered wakes if that callback replaces this observer.
                if !delivery_active.load(Ordering::Acquire) {
                    break;
                }
                // Foreground scheduling avoids synchronous App re-entry.
                // Other accessibility options also post this notification;
                // never interpret it as a toggle or use a stale sample.
                callback(current());
            }
        });
        Self {
            center,
            token,
            active,
            _delivery: delivery,
        }
    }
}

impl Drop for ReducedMotionObserver {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        // SAFETY: MacPlatform and its observer are main-thread-owned. Both
        // objects were retained during registration. Removing the token
        // releases the copied block; dropping _delivery cancels queued work.
        unsafe {
            let _: () = msg_send![self.center, removeObserver: self.token];
            let _: () = msg_send![self.token, release];
            let _: () = msg_send![self.center, release];
        }
    }
}
