//! File drops, delivered.
//!
//! GPUI's cross-platform file drop is [`gpui::PlatformInput::FileDrop`], which
//! carries `ExternalPaths` -- filesystem paths. A browser never gives us one: a
//! `drop` event exposes `File` objects, whose bytes are readable but whose
//! location is not. So the web platform historically intercepted `dragover` and
//! `drop` only to stop the browser navigating to the dropped file, and threw the
//! payload away.
//!
//! Rather than synthesize a path that no consumer could open, the drop is
//! delivered on its own channel with the bytes already read. A host that wants
//! web file drops subscribes with [`web_file_drops`] and turns each
//! [`WebFileDrop`] into whatever its native `FileDrop` handler would have
//! produced -- an attachment, a document, a task -- so the two paths converge in
//! the host rather than in a fake path.
//!
//! Upstream note: the honest fix is a `PlatformInput::FileDrop` variant that can
//! carry bytes instead of paths, which is a GPUI API change. This channel is the
//! same delivery without that change.

use std::cell::RefCell;

use gpui::{Pixels, Point};

/// A file dropped on the window, with its contents already read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebFileDrop {
    /// The file name as the browser reported it. It is a name, not a path:
    /// there is no directory component and it is attacker-controlled, so treat
    /// it as a label rather than as a place to write.
    pub name: String,
    /// The browser's MIME type, empty when it could not determine one.
    pub mime: String,
    /// The whole file. Browsers hand out files by value, so there is no
    /// streaming variant to prefer here.
    pub bytes: Vec<u8>,
    /// Where the drop landed, in window coordinates, so a host can route it to
    /// the surface under the pointer.
    pub position: Point<Pixels>,
}

thread_local! {
    /// One process-wide channel. The web has exactly one window and one main
    /// thread, and the drop listener is installed by the window rather than by
    /// the host, so the host needs somewhere to reach that does not require
    /// holding the window.
    static CHANNEL: RefCell<Option<(
        async_channel::Sender<WebFileDrop>,
        async_channel::Receiver<WebFileDrop>,
    )>> = const { RefCell::new(None) };
}

fn with_channel<R>(
    read: impl FnOnce(
        &(
            async_channel::Sender<WebFileDrop>,
            async_channel::Receiver<WebFileDrop>,
        ),
    ) -> R,
) -> R {
    CHANNEL.with(|cell| {
        let mut cell = cell.borrow_mut();
        let channel = cell.get_or_insert_with(async_channel::unbounded);
        read(channel)
    })
}

/// Subscribe to file drops on the web window.
///
/// The channel is unbounded and the receiver is cloneable, so subscribing more
/// than once is allowed; each subscriber competes for drops rather than each
/// receiving a copy, which matches the single-consumer shape a host wants.
pub fn web_file_drops() -> async_channel::Receiver<WebFileDrop> {
    with_channel(|(_, receiver)| receiver.clone())
}

pub(crate) fn deliver(drop: WebFileDrop) {
    // Unbounded, so this never blocks; a failure means every receiver is gone,
    // which is the ordinary state of a host that does not want file drops.
    let _ = with_channel(|(sender, _)| sender.try_send(drop));
}
