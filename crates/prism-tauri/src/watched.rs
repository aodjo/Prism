//! What a machine shows while somebody is watching it.
//!
//! A handle at the left edge of its screen — the same one a stream window shows when it fills
//! the screen — that opens who is watching and the two ways to end it: send them away, or stop
//! sharing. The home window's Disconnect does the first, but the home window may be closed,
//! behind everything or on another desktop, and the person whose machine this is should never
//! have to go looking for how to get it back.
//!
//! The machine watching does not see it. The panel refuses to be captured, so the handle is not
//! drawn into the picture and mistaken over there for one of theirs — while every other window
//! of Prism's, the home window included, is sent like anything else on the screen.

use tauri::AppHandle;

/// How often the sharing state is looked at to decide whether the handle should be showing.
///
/// Twice a second: it appears within half a second of somebody starting to watch, which is
/// sooner than anybody sitting here notices they are being watched any other way.
#[cfg(target_os = "macos")]
const LOOK_EVERY: std::time::Duration = std::time::Duration::from_millis(500);

/// Shows the handle whenever somebody is watching this machine, for as long as Prism runs.
#[cfg(target_os = "macos")]
pub fn watch(app: &AppHandle) {
    let app = app.clone();

    std::thread::spawn(move || {
        let mut showing: Option<String> = None;

        loop {
            std::thread::sleep(LOOK_EVERY);

            let now = crate::sharing::watcher(&app);

            if now == showing {
                continue;
            }

            showing.clone_from(&now);

            let on_main = app.clone();
            let _ = app.run_on_main_thread(move || panel::present(&on_main, now));
        }
    });
}

/// Does nothing yet, on a system with no handle to show.
///
/// The home window's Disconnect is there on every system. A handle at the edge of the screen is
/// macOS's for now, because it is built from AppKit's panels.
#[cfg(not(target_os = "macos"))]
pub fn watch(app: &AppHandle) {
    let _ = app;
}

#[cfg(target_os = "macos")]
mod panel {
    use std::cell::RefCell;

    use objc2::MainThreadOnly;
    use objc2::rc::Retained;
    use objc2_app_kit::{
        NSBackingStoreType, NSColor, NSPanel, NSScreen, NSStatusWindowLevel,
        NSWindowCollectionBehavior, NSWindowSharingType, NSWindowStyleMask,
    };
    use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};
    use prism_stream::drawer::{Anchor, Drawer, Entry};
    use tauri::AppHandle;

    /// The panel and the drawer in it, once there has been somebody to show them for.
    struct Shown {
        panel: Retained<NSPanel>,
        drawer: Drawer,
    }

    thread_local! {
        /// Built the first time somebody watches and kept, hidden, for the next.
        ///
        /// On the main thread, which is the only one that ever reaches it: every call here is
        /// run there.
        static SHOWN: RefCell<Option<Shown>> = const { RefCell::new(None) };
    }

    /// Shows the handle for somebody watching, or hides it when nobody is.
    pub fn present(app: &AppHandle, watcher: Option<String>) {
        SHOWN.with(|slot| {
            let mut slot = slot.borrow_mut();

            let Some(name) = watcher else {
                if let Some(shown) = slot.as_ref() {
                    shown.drawer.set_shown(false);
                    shown.panel.orderOut(None);
                }

                return;
            };

            if slot.is_none() {
                *slot = build(app);
            }

            if let Some(shown) = slot.as_ref() {
                shown
                    .drawer
                    .set_heading(&format!("{name}에서 이 컴퓨터를 보는 중"));
                shown.drawer.set_shown(true);
                place(&shown.panel);
                shown.panel.orderFrontRegardless();
            }
        });
    }

    /// Builds the panel: see-through, above everything, on every desktop, never taking focus.
    ///
    /// Never taking focus is the point of a panel rather than a window. Pressing the handle
    /// must not pull this application in front of whatever the person here was doing.
    fn build(app: &AppHandle) -> Option<Shown> {
        let marker = MainThreadMarker::new()?;

        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(marker),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(48.0, 48.0)),
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );

        panel.setFloatingPanel(true);
        panel.setLevel(NSStatusWindowLevel);
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(false);
        panel.setHidesOnDeactivate(false);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        // Left out of every capture, this one's included. The handle is for the person sitting
        // here; drawn into the picture it would be a second handle on the other machine's
        // screen, one that does nothing there.
        panel.setSharingType(NSWindowSharingType::None);
        // SAFETY: the panel is held in `SHOWN` for the life of the process and never closed,
        // only ordered out, so it is never released out from under that.
        unsafe { panel.setReleasedWhenClosed(false) };

        let content = panel.contentView()?;
        let entries = [
            Entry {
                symbol: "xmark.circle",
                label: "연결 끊기".to_owned(),
            },
            Entry {
                symbol: "stop.circle",
                label: "공유 중지".to_owned(),
            },
        ];

        let acting = app.clone();
        let drawer = Drawer::install(
            &content,
            Some(""),
            &entries,
            true,
            Anchor::Middle,
            move |index| act(&acting, index),
        )?;

        Some(Shown { panel, drawer })
    }

    /// Does what an entry in the drawer says.
    ///
    /// Stopping waits for the session to end, which is off this thread: it is the one drawing
    /// every window, and a second's wait there is a second of everything standing still.
    fn act(app: &AppHandle, index: usize) {
        use tauri::Manager as _;

        match index {
            0 => {
                let _ = crate::sharing::disconnect_viewer(app.state());
            }
            1 => {
                let app = app.clone();

                std::thread::spawn(move || {
                    let _ = crate::sharing::stop_sharing(app.state(), app.state());
                });
            }
            _ => {}
        }
    }

    /// Puts the panel against the left edge of the main display, halfway up.
    ///
    /// The main display because it is the one being sent. Placed each time it is shown, so a
    /// display rearranged since last time is not a handle off in a corner of the old layout.
    fn place(panel: &NSPanel) {
        let Some(marker) = MainThreadMarker::new() else {
            return;
        };
        let Some(screen) = NSScreen::screens(marker).firstObject() else {
            return;
        };

        let visible = screen.visibleFrame();
        let size = panel.frame().size;

        panel.setFrameOrigin(NSPoint::new(
            visible.origin.x,
            visible.origin.y + (visible.size.height - size.height) / 2.0,
        ));
    }
}
