//! What a machine shows while somebody is controlling it.
//!
//! Prism's own windows get out of the way the moment a session opens: the person here did not
//! open them, and the person over there came to see the desktop, not Prism. What stays is a
//! banner at the top of the screen saying who is in control, with a button that ends it.
//!
//! The banner is there when it is needed and not otherwise. It appears as the session opens,
//! and again whenever the person sitting here takes hold of their own mouse — the moment they
//! might be wondering why the pointer is moving by itself — and fades a few seconds after they
//! let go. The machine watching does not see it: the panel refuses to be captured.

use tauri::AppHandle;

/// How often the sharing state is looked at.
///
/// Four times a second: often enough that the banner answers a hand on the mouse without a lag
/// anybody notices, and each look is a lock taken and let go.
const LOOK_EVERY: std::time::Duration = std::time::Duration::from_millis(250);

/// Watches for somebody controlling this machine, for as long as Prism runs.
///
/// Its own thread, which only ever looks: everything it changes on screen is handed to the main
/// thread, which is the only one allowed to.
pub fn watch(app: &AppHandle) {
    let app = app.clone();

    #[cfg(target_os = "macos")]
    let _ = app.run_on_main_thread(banner::listen);

    std::thread::spawn(move || {
        let mut was: Option<String> = None;

        loop {
            std::thread::sleep(LOOK_EVERY);

            let now = crate::sharing::watcher(&app);

            if was.is_none() && now.is_some() {
                let on_main = app.clone();
                let _ = app.run_on_main_thread(move || step_aside(&on_main));
            }

            // Every look while a session is open, and one more when it closes. The banner's own
            // timing — how long since the mouse was touched — is decided where it is drawn.
            #[cfg(target_os = "macos")]
            if now.is_some() || was.is_some() {
                let on_main = app.clone();
                let showing = now.clone();
                let _ = app.run_on_main_thread(move || banner::tick(&on_main, showing));
            }

            was = now;
        }
    });
}

/// Hides every window Prism has open, as a session opens.
///
/// Hidden rather than closed, so each comes back from the menu bar exactly as it was.
fn step_aside(app: &AppHandle) {
    use tauri::Manager as _;

    for window in app.webview_windows().values() {
        let _ = window.hide();
    }
}

#[cfg(target_os = "macos")]
mod banner {
    use std::cell::{Cell, RefCell};
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
    use objc2_app_kit::{
        NSAnimatablePropertyContainer, NSAnimationContext, NSAppearance, NSAppearanceCustomization,
        NSAppearanceNameDarkAqua, NSBackingStoreType, NSBox, NSBoxType, NSButton, NSColor, NSEvent,
        NSEventMask, NSFont, NSPanel, NSScreen, NSStatusWindowLevel, NSTextField, NSTitlePosition,
        NSWindowCollectionBehavior, NSWindowSharingType, NSWindowStyleMask,
    };
    use objc2_core_graphics::{CGEvent, CGEventField};
    use objc2_foundation::{
        MainThreadMarker, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
    };
    use prism_core::input::macos::INJECTED;
    use tauri::AppHandle;

    /// How long the banner stays after the mouse was last touched here.
    const FADE_AFTER: Duration = Duration::from_secs(3);

    /// How long it takes to fade.
    const FADING: f64 = 0.8;

    /// How long it takes to appear. Quick, because it is answering a hand.
    const APPEARING: f64 = 0.15;

    /// How wide the banner is.
    const WIDTH: f64 = 470.0;

    /// How tall the banner is.
    const HEIGHT: f64 = 46.0;

    /// How far below the top of the screen it sits.
    const DROP: f64 = 12.0;

    /// When the mouse here was last touched, in milliseconds since the Unix epoch.
    ///
    /// Written from the event monitors and read when deciding what to show. An atomic because
    /// the two are both on the main thread today and nothing should depend on that staying so.
    static TOUCHED: AtomicU64 = AtomicU64::new(0);

    thread_local! {
        /// The banner, once there has been a session to show it for.
        static BANNER: RefCell<Option<Banner>> = const { RefCell::new(None) };

        /// The monitors watching the mouse, held so they stay installed.
        static MONITORS: RefCell<Vec<Retained<AnyObject>>> = const { RefCell::new(Vec::new()) };
    }

    define_class!(
        // SAFETY:
        // - `NSObject` has no subclassing requirements.
        // - `Target` does not implement `Drop`.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = AppHandle]
        struct Target;

        unsafe impl NSObjectProtocol for Target {}

        impl Target {
            /// Ends the session, leaving the machine shared.
            #[unsafe(method(disconnect:))]
            fn disconnect(&self, _sender: &NSButton) {
                use tauri::Manager as _;

                let _ = crate::sharing::disconnect_viewer(self.ivars().state());
            }
        }
    );

    /// The banner and what it needs to change about itself.
    struct Banner {
        panel: Retained<NSPanel>,
        line: Retained<NSTextField>,
        /// Kept alive here, because a button does not keep its target.
        _target: Retained<Target>,
        /// Whether a session is open, so its start is noticed once.
        session: Cell<bool>,
        /// Whether it is showing or on its way to showing.
        shown: Cell<bool>,
    }

    impl Banner {
        /// Brings it up, if it is not up already.
        fn appear(&self) {
            if self.shown.replace(true) {
                return;
            }

            place(&self.panel);
            self.panel.setIgnoresMouseEvents(false);
            self.panel.orderFrontRegardless();
            animate(&self.panel, 1.0, APPEARING);
        }

        /// Lets it fade, if it is showing.
        ///
        /// It stops taking clicks as it starts to fade, so a banner on its way out never
        /// catches one meant for whatever is behind it.
        fn fade(&self) {
            if !self.shown.replace(false) {
                return;
            }

            self.panel.setIgnoresMouseEvents(true);
            animate(&self.panel, 0.0, FADING);
        }

        /// Whether the pointer is over it, which keeps it up for as long as it is.
        fn hovered(&self) -> bool {
            let at = NSEvent::mouseLocation();
            let frame = self.panel.frame();

            self.shown.get()
                && at.x >= frame.origin.x
                && at.x <= frame.origin.x + frame.size.width
                && at.y >= frame.origin.y
                && at.y <= frame.origin.y + frame.size.height
        }
    }

    /// Starts watching the mouse here, for the rest of the run.
    ///
    /// Two monitors, because the system hands each only half the events: one sees what goes to
    /// other applications, the other what goes to this one — the banner itself among them. What
    /// the injector put there carries its mark and is left out: that is the far side's hand.
    pub fn listen() {
        let mask = NSEventMask::MouseMoved
            | NSEventMask::LeftMouseDown
            | NSEventMask::RightMouseDown
            | NSEventMask::LeftMouseDragged
            | NSEventMask::RightMouseDragged
            | NSEventMask::ScrollWheel;

        let elsewhere = RcBlock::new(|event: NonNull<NSEvent>| {
            // SAFETY: the system hands over an event that is alive for the length of the call.
            noticed(unsafe { event.as_ref() });
        });
        let here = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: as above; and the event is handed back unchanged, which is what lets it
            // go on to wherever it was going.
            noticed(unsafe { event.as_ref() });
            event.as_ptr()
        });

        let global = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &elsewhere);
        // SAFETY: the handler returns the event it was given, which is a valid pointer.
        let local = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &here) };

        MONITORS.with(|kept| {
            kept.borrow_mut().extend(global.into_iter().chain(local));
        });
    }

    /// Records that the mouse was touched here, unless it was the far side that moved it.
    fn noticed(event: &NSEvent) {
        let injected = event.CGEvent().is_some_and(|raw| {
            CGEvent::integer_value_field(Some(&raw), CGEventField::EventSourceUserData) == INJECTED
        });

        if !injected {
            TOUCHED.store(now_ms(), Ordering::Relaxed);
        }
    }

    /// Shows the banner for a session, fades it, or takes it away when the session is over.
    pub fn tick(app: &AppHandle, watcher: Option<String>) {
        BANNER.with(|slot| {
            let mut slot = slot.borrow_mut();

            let Some(name) = watcher else {
                if let Some(banner) = slot.as_ref() {
                    banner.session.set(false);
                    banner.fade();
                }

                return;
            };

            if slot.is_none() {
                *slot = build(app);
            }

            let Some(banner) = slot.as_ref() else {
                return;
            };

            // Up as the session opens, whatever the mouse is doing, so the person here is told
            // once without having to reach for it.
            if !banner.session.replace(true) {
                TOUCHED.store(now_ms(), Ordering::Relaxed);
            }

            banner.line.setStringValue(&NSString::from_str(&format!(
                "{name}에서 이 컴퓨터를 제어하는 중입니다"
            )));

            let since = now_ms().saturating_sub(TOUCHED.load(Ordering::Relaxed));

            if u128::from(since) < FADE_AFTER.as_millis() || banner.hovered() {
                banner.appear();
            } else {
                banner.fade();
            }
        });
    }

    /// Builds the banner: a dark bar with who is in control and the button that ends it.
    ///
    /// A panel that never takes focus, above everything, on every desktop, and see-through
    /// around its rounded corners.
    fn build(app: &AppHandle) -> Option<Banner> {
        let marker = MainThreadMarker::new()?;

        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(marker),
            rect(0.0, 0.0, WIDTH, HEIGHT),
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );

        panel.setFloatingPanel(true);
        panel.setLevel(NSStatusWindowLevel);
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(true);
        panel.setHidesOnDeactivate(false);
        panel.setAlphaValue(0.0);
        panel.setIgnoresMouseEvents(true);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        // Left out of every capture: it is for the person sitting here, and drawn into the
        // picture it would cover part of the screen the other machine came to see.
        panel.setSharingType(NSWindowSharingType::None);
        // SAFETY: the panel is held in `BANNER` for the life of the process and never closed,
        // only faded, so it is never released out from under that.
        unsafe { panel.setReleasedWhenClosed(false) };

        let content = panel.contentView()?;

        let bar = NSBox::initWithFrame(NSBox::alloc(marker), rect(0.0, 0.0, WIDTH, HEIGHT));
        bar.setBoxType(NSBoxType::Custom);
        bar.setTitlePosition(NSTitlePosition::NoTitle);
        bar.setContentViewMargins(NSSize::new(0.0, 0.0));
        bar.setCornerRadius(12.0);
        bar.setBorderWidth(1.0);
        bar.setBorderColor(&NSColor::colorWithWhite_alpha(1.0, 0.12));
        bar.setFillColor(&NSColor::colorWithWhite_alpha(0.1, 0.95));
        // SAFETY: a constant the framework defines, read once it has been linked.
        let dark = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua });
        bar.setAppearance(dark.as_deref());
        content.addSubview(&bar);

        let inside = bar.contentView()?;

        let line = NSTextField::labelWithString(&NSString::from_str(""), marker);
        line.setFont(Some(&NSFont::systemFontOfSize(13.0)));
        line.setTextColor(Some(&NSColor::labelColor()));
        line.setFrame(rect(18.0, (HEIGHT - 18.0) / 2.0, WIDTH - 150.0, 18.0));
        inside.addSubview(&line);

        let target = Target::alloc(marker).set_ivars(app.clone());
        // SAFETY: `init` on `NSObject` takes no arguments and returns the object it was sent
        // to, and the instance variables it needs were set on the allocation above.
        let target: Retained<Target> = unsafe { msg_send![super(target), init] };

        // SAFETY: the target is not retained by the button; `Banner` holds it for as long as
        // the button exists. The selector is one the class defines, taking the sender.
        let button = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("연결 끊기"),
                Some(&target),
                Some(sel!(disconnect:)),
                marker,
            )
        };
        let size = button.fittingSize();
        button.setFrame(rect(
            WIDTH - size.width - 12.0,
            (HEIGHT - size.height) / 2.0,
            size.width,
            size.height,
        ));
        inside.addSubview(&button);

        Some(Banner {
            panel,
            line,
            _target: target,
            session: Cell::new(false),
            shown: Cell::new(false),
        })
    }

    /// Puts the banner at the top of the main display, in the middle, below the menu bar.
    ///
    /// The main display because it is the one being sent, and so the one somebody sitting here
    /// sees their pointer being moved on. Placed each time it appears, so a display rearranged
    /// since is not a banner left in a corner of the old layout.
    fn place(panel: &NSPanel) {
        let Some(marker) = MainThreadMarker::new() else {
            return;
        };
        let Some(screen) = NSScreen::screens(marker).firstObject() else {
            return;
        };

        let visible = screen.visibleFrame();

        panel.setFrameOrigin(NSPoint::new(
            visible.origin.x + (visible.size.width - WIDTH) / 2.0,
            visible.origin.y + visible.size.height - HEIGHT - DROP,
        ));
    }

    /// Moves the panel's opacity to `alpha` over `seconds`.
    fn animate(panel: &NSPanel, alpha: f64, seconds: f64) {
        NSAnimationContext::beginGrouping();
        NSAnimationContext::currentContext().setDuration(seconds);
        panel.animator().setAlphaValue(alpha);
        NSAnimationContext::endGrouping();
    }

    /// The time, in milliseconds since the Unix epoch.
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            })
    }

    /// Builds a rectangle from its corner and size.
    fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
    }
}
