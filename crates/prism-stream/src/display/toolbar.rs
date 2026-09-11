//! The row of controls in the stream window's title bar.
//!
//! Drawn by the window server rather than by this program. Everything else the window shows is
//! rasterised and blended over the video, which is right for numbers that belong on top of the
//! picture and wrong for controls: a toolbar drawn over the stream would cover the thing
//! somebody came to look at, and would have to grow its own hit testing, its own hover states
//! and its own idea of what a button looks like on this machine.
//!
//! An `NSToolbar` costs none of that. It sits in the title bar beside the window's own buttons,
//! it is the size and shape every other application's is, and a press arrives as a message.
//!
//! # Threading
//!
//! Everything here runs on the main thread, which is where the window and its event loop
//! already are. The pressed queue is behind a lock anyway because the type has to be `Send` to
//! be held beside the rest of the session state.

#![cfg(target_os = "macos")]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSEvent, NSImage, NSMenu, NSMenuItem, NSToolbar, NSToolbarDelegate, NSToolbarDisplayMode,
    NSToolbarItem, NSToolbarItemIdentifier, NSWindow, NSWindowStyleMask, NSWindowToolbarStyle,
};
use objc2_foundation::{MainThreadMarker, NSArray, NSObject, NSObjectProtocol, NSPoint, NSString};
use sdl3::video::Window;
use sdl3_sys::properties::SDL_GetPointerProperty;
use sdl3_sys::video::{SDL_GetWindowProperties, SDL_PROP_WINDOW_COCOA_WINDOW_POINTER};

use crate::drawer::{Drawer, Entry};

/// One control in the title bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// Hand the pointer and the keyboard to the machine being watched, or take them back.
    Control,
    /// Make the window the size of the picture arriving in it.
    Fit,
    /// Fill the screen, and leave it again.
    Fullscreen,
    /// Send a file to the machine being watched.
    Send,
    /// Ask the machine being watched what it is offering.
    Fetch,
    /// End the session.
    Disconnect,
}

impl Tool {
    /// The name the window server knows this item by.
    fn identifier(self) -> &'static str {
        match self {
            Tool::Control => "kr.presm.prism.control",
            Tool::Fit => "kr.presm.prism.fit",
            Tool::Fullscreen => "kr.presm.prism.fullscreen",
            Tool::Send => "kr.presm.prism.send",
            Tool::Fetch => "kr.presm.prism.fetch",
            Tool::Disconnect => "kr.presm.prism.disconnect",
        }
    }

    /// The tool an identifier names, if it is one of these.
    fn named(identifier: &str) -> Option<Self> {
        TOOLS
            .iter()
            .copied()
            .find(|one| one.identifier() == identifier)
    }

    /// What the item is called underneath its picture.
    fn label(self) -> &'static str {
        match self {
            Tool::Control => "제어",
            Tool::Fit => "원본 크기",
            Tool::Fullscreen => "전체 화면",
            Tool::Send => "파일 보내기",
            Tool::Fetch => "파일 가져오기",
            Tool::Disconnect => "연결 끊기",
        }
    }

    /// The system symbol drawn on the item.
    ///
    /// A name the system does not know draws nothing, and the item falls back to its label,
    /// which is why every one of these is a symbol that has existed since the toolbar style
    /// this window uses did.
    fn symbol(self) -> &'static str {
        match self {
            Tool::Control => "cursorarrow",
            Tool::Fit => "arrow.down.right.and.arrow.up.left",
            Tool::Fullscreen => "arrow.up.left.and.arrow.down.right",
            Tool::Send => "square.and.arrow.up",
            Tool::Fetch => "square.and.arrow.down",
            Tool::Disconnect => "power",
        }
    }
}

/// The controls, in the order they appear.
const TOOLS: [Tool; 6] = [
    Tool::Control,
    Tool::Fit,
    Tool::Fullscreen,
    Tool::Send,
    Tool::Fetch,
    Tool::Disconnect,
];

/// The symbol the control item carries while the machine is being controlled.
const CONTROLLING: &str = "cursorarrow.rays";

/// The controls the drawer holds while the window fills the screen, in order.
///
/// Not the toolbar's list: fitting the window to the picture means nothing on a window that is
/// the size of the screen, and the one that fills the screen is, here, the one that leaves it.
const DRAWN: [Tool; 5] = [
    Tool::Control,
    Tool::Fullscreen,
    Tool::Send,
    Tool::Fetch,
    Tool::Disconnect,
];

/// What a control in the drawer says and shows.
fn drawn(tool: Tool, controlling: bool) -> Entry {
    match tool {
        Tool::Fullscreen => Entry {
            symbol: "arrow.down.right.and.arrow.up.left",
            label: "전체 화면 나가기".to_owned(),
        },
        Tool::Control if controlling => Entry {
            symbol: CONTROLLING,
            label: tool.label().to_owned(),
        },
        _ => Entry {
            symbol: tool.symbol(),
            label: tool.label().to_owned(),
        },
    }
}

/// What the delegate holds.
struct Held {
    /// Presses nobody has read yet.
    pressed: Arc<Mutex<VecDeque<Tool>>>,
    /// Files chosen from the menu that nobody has read yet.
    chosen: Arc<Mutex<VecDeque<String>>>,
    /// The items, kept so the toolbar can be asked for them and so one can be redrawn.
    ///
    /// `RefCell` rather than a lock: the class is main-thread only, and every path that
    /// reaches this is the window's own.
    items: RefCell<Vec<(Tool, Retained<NSToolbarItem>)>>,
}

define_class!(
    // SAFETY:
    // - `NSObject` has no subclassing requirements.
    // - `Controls` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Held]
    struct Controls;

    unsafe impl NSObjectProtocol for Controls {}

    unsafe impl NSToolbarDelegate for Controls {
        /// Returns the items this toolbar starts with.
        #[unsafe(method_id(toolbarDefaultItemIdentifiers:))]
        fn default_items(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSToolbarItemIdentifier>> {
            self.identifiers()
        }

        /// Returns every item this toolbar can hold, which is the same list.
        #[unsafe(method_id(toolbarAllowedItemIdentifiers:))]
        fn allowed_items(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSToolbarItemIdentifier>> {
            self.identifiers()
        }

        /// Hands over the item an identifier names.
        #[unsafe(method_id(toolbar:itemForItemIdentifier:willBeInsertedIntoToolbar:))]
        fn item_for(
            &self,
            _toolbar: &NSToolbar,
            identifier: &NSToolbarItemIdentifier,
            _inserted: bool,
        ) -> Option<Retained<NSToolbarItem>> {
            Tool::named(&identifier.to_string()).and_then(|wanted| {
                self.ivars()
                    .items
                    .borrow()
                    .iter()
                    .find(|(tool, _)| *tool == wanted)
                    .map(|(_, item)| item.clone())
            })
        }
    }

    impl Controls {
        /// Records which file was chosen from the menu of what the far machine has.
        ///
        /// The name is the item's title. It came from the far machine and has already been
        /// refused by the wire if it was not a file name, so what is put back on the queue is
        /// the same string the listing carried.
        #[unsafe(method(chose:))]
        fn chose(&self, sender: &NSMenuItem) {
            if let Ok(mut queue) = self.ivars().chosen.lock() {
                queue.push_back(sender.title().to_string());
            }
        }

        /// Records that one of the items was pressed.
        ///
        /// The window's loop reads the queue on its next turn rather than acting here, so a
        /// press never runs the session's work inside an event the window server is waiting
        /// on the return of.
        #[unsafe(method(pressed:))]
        fn pressed(&self, sender: &NSToolbarItem) {
            let Some(tool) = Tool::named(&sender.itemIdentifier().to_string()) else {
                return;
            };

            if let Ok(mut queue) = self.ivars().pressed.lock() {
                queue.push_back(tool);
            }
        }
    }
);

impl Controls {
    /// Returns the identifiers of every item, in order.
    fn identifiers(&self) -> Retained<NSArray<NSToolbarItemIdentifier>> {
        let names: Vec<Retained<NSString>> = TOOLS
            .iter()
            .map(|tool| NSString::from_str(tool.identifier()))
            .collect();

        NSArray::from_retained_slice(&names)
    }
}

/// The controls in a window's title bar, and the presses they have collected.
///
/// And the same controls in a drawer at the left edge, for while the window fills the screen:
/// the title bar is gone then, and so is everything that was in it. Only the picture shows,
/// with a handle at the edge that opens the controls beside it.
pub struct Toolbar {
    pressed: Arc<Mutex<VecDeque<Tool>>>,
    chosen: Arc<Mutex<VecDeque<String>>>,
    controls: Retained<Controls>,
    toolbar: Retained<NSToolbar>,
    window: Retained<NSWindow>,
    /// The drawer, once the window has something under it to be laid over.
    drawer: Option<Drawer>,
    /// Whether the window was filling the screen the last time anybody looked.
    filling: Cell<bool>,
}

impl Toolbar {
    /// Puts the controls in the window's title bar.
    ///
    /// Returns `None` on a window with no title bar to put them in, and on a build where the
    /// window is not the main thread's — neither of which happens here, and both of which are
    /// better ignored than panicked over: a session without a toolbar is a session.
    #[must_use]
    pub fn install(window: &Window) -> Option<Self> {
        let marker = MainThreadMarker::new()?;
        let ns_window = cocoa_window(window)?;

        let pressed = Arc::new(Mutex::new(VecDeque::new()));
        let chosen = Arc::new(Mutex::new(VecDeque::new()));
        let controls = Controls::alloc(marker).set_ivars(Held {
            pressed: Arc::clone(&pressed),
            chosen: Arc::clone(&chosen),
            items: RefCell::new(Vec::new()),
        });
        // SAFETY: `init` on `NSObject` takes no arguments and returns the object it was sent
        // to, and the instance variables it needs were set on the allocation above.
        let controls: Retained<Controls> = unsafe { msg_send![super(controls), init] };

        let items = TOOLS
            .iter()
            .map(|tool| (*tool, item(*tool, &controls, marker)))
            .collect();
        controls.ivars().items.replace(items);

        let toolbar = NSToolbar::initWithIdentifier(
            NSToolbar::alloc(marker),
            &NSString::from_str("kr.presm.prism.stream"),
        );
        let delegate = ProtocolObject::from_ref(&*controls);

        toolbar.setDelegate(Some(delegate));
        toolbar.setDisplayMode(NSToolbarDisplayMode::IconOnly);
        toolbar.setAllowsUserCustomization(false);

        // Beside the title rather than under it, which is the shape the window has room for:
        // the picture starts immediately below, and a two-storey title bar would take a strip
        // of it away for no more than what one row already says.
        ns_window.setToolbarStyle(NSWindowToolbarStyle::Unified);
        ns_window.setToolbar(Some(&toolbar));

        Some(Self {
            pressed,
            chosen,
            controls,
            toolbar,
            window: ns_window,
            drawer: None,
            filling: Cell::new(false),
        })
    }

    /// Puts the drawer the controls move into while the window fills the screen.
    ///
    /// Separate from [`Toolbar::install`] and after the picture's own view exists, because
    /// views stack in the order they are added: one added before the picture's would be under
    /// it, and a drawer nobody can see is not a way to leave full screen.
    pub fn add_drawer(&mut self) {
        let Some(content) = self.window.contentView() else {
            return;
        };

        let entries: Vec<Entry> = DRAWN.iter().map(|tool| drawn(*tool, false)).collect();
        let pressed = Arc::clone(&self.pressed);

        self.drawer = Drawer::install(&content, None, &entries, false, move |index| {
            if let (Some(tool), Ok(mut queue)) = (DRAWN.get(index), pressed.lock()) {
                queue.push_back(*tool);
            }
        });
    }

    /// Returns whether the window fills the screen, and moves the controls to match.
    ///
    /// Asked of the window every turn rather than remembered from the control that asked for
    /// it, because that control is not the only way in or out: the green button, the menu and
    /// Escape all change it without a word to anything here.
    pub fn sync_fullscreen(&self) -> bool {
        let filling = self
            .window
            .styleMask()
            .contains(NSWindowStyleMask::FullScreen);

        if filling != self.filling.replace(filling) {
            self.toolbar.setVisible(!filling);

            if let Some(drawer) = self.drawer.as_ref() {
                drawer.set_shown(filling);
            }
        }

        filling
    }

    /// Returns whether a place in the window is over the drawer.
    ///
    /// In the window's own coordinates, which count down from the top as the pointer's do. A
    /// click there is a click on a control, and must not reach the far machine as well.
    #[must_use]
    pub fn covers(&self, x: f32, y: f32) -> bool {
        let (Some(drawer), Some(content)) = (self.drawer.as_ref(), self.window.contentView())
        else {
            return false;
        };

        let height = content.bounds().size.height;

        drawer.covers(NSPoint::new(f64::from(x), height - f64::from(y)))
    }

    /// Returns the next control that was pressed, or `None` if none was.
    pub fn pressed(&self) -> Option<Tool> {
        self.pressed.lock().ok()?.pop_front()
    }

    /// Returns the next file chosen from the menu, or `None` if none was.
    pub fn chosen(&self) -> Option<String> {
        self.chosen.lock().ok()?.pop_front()
    }

    /// Shows what the far machine is offering, under the item that asked for it.
    ///
    /// A menu rather than a window, because this is a short list of names attached to the
    /// control that produced it — and because a window would need a list, a scrollbar and a
    /// way to close it, none of which this process has and all of which the menu bar does.
    ///
    /// An empty listing still opens, saying that it is empty. A menu that did not appear would
    /// be indistinguishable from a button that does nothing.
    pub fn offer(&self, files: &[(String, u64)], more: bool) {
        let Some(marker) = MainThreadMarker::new() else {
            return;
        };

        let menu = NSMenu::initWithTitle(NSMenu::alloc(marker), &NSString::from_str(""));

        // SAFETY: every selector named here is one this class defines and every one of them
        // takes the single sender argument the menu sends with it. The strings are live for
        // the duration of each call, and the menu copies what it keeps.
        unsafe {
            if files.is_empty() {
                let empty = NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(marker),
                    &NSString::from_str("가져올 파일이 없습니다"),
                    None,
                    &NSString::from_str(""),
                );

                empty.setEnabled(false);
                menu.addItem(&empty);
            }

            for (name, size) in files {
                let item = NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(marker),
                    &NSString::from_str(name),
                    Some(sel!(chose:)),
                    &NSString::from_str(""),
                );

                // Not retained, which is the usual Objective-C rule; what keeps it alive is
                // the toolbar holding the same object as its delegate.
                item.setTarget(Some(&*self.controls));
                item.setToolTip(Some(&NSString::from_str(&plainly(*size))));
                menu.addItem(&item);
            }

            if more {
                let rest = NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(marker),
                    &NSString::from_str("…그 밖에 더 있습니다"),
                    None,
                    &NSString::from_str(""),
                );

                rest.setEnabled(false);
                menu.addItem(&rest);
            }
        }

        // Where the pointer is, which is over the control that was just pressed. In screen
        // coordinates, which is what this takes when it is given no view to measure against.
        menu.popUpMenuPositioningItem_atLocation_inView(None, NSEvent::mouseLocation(), None);
    }

    /// Redraws the control item to say whether the far machine is being controlled.
    pub fn set_controlling(&self, controlling: bool) {
        let symbol = if controlling {
            CONTROLLING
        } else {
            Tool::Control.symbol()
        };

        for (tool, item) in self.controls.ivars().items.borrow().iter() {
            if *tool == Tool::Control {
                item.setImage(symbol_image(symbol).as_deref());
            }
        }

        if let Some(drawer) = self.drawer.as_ref() {
            drawer.set_entry(0, &drawn(Tool::Control, controlling));
        }
    }
}

impl core::fmt::Debug for Toolbar {
    /// Names the type and how many presses are waiting, without reaching into the window.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let waiting = self.pressed.lock().map(|queue| queue.len()).unwrap_or(0);

        f.debug_struct("Toolbar")
            .field("waiting", &waiting)
            .finish_non_exhaustive()
    }
}

/// Renders a size the way somebody reads one.
fn plainly(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    match bytes {
        0..KB => format!("{bytes} B"),
        KB..MB => format!("{:.0} KB", bytes as f64 / KB as f64),
        MB..GB => format!("{:.1} MB", bytes as f64 / MB as f64),
        _ => format!("{:.2} GB", bytes as f64 / GB as f64),
    }
}

/// Builds one item, pointed at the object that collects presses.
fn item(tool: Tool, controls: &Controls, marker: MainThreadMarker) -> Retained<NSToolbarItem> {
    let identifier = NSString::from_str(tool.identifier());
    let label = NSString::from_str(tool.label());

    let item = NSToolbarItem::initWithItemIdentifier(NSToolbarItem::alloc(marker), &identifier);

    item.setLabel(&label);
    item.setToolTip(Some(&label));
    item.setImage(symbol_image(tool.symbol()).as_deref());

    // SAFETY: the target is not retained, which is the usual Objective-C rule; what keeps it
    // alive is the toolbar holding the same object as its delegate for as long as the window
    // has a toolbar at all. The selector is one this class defines, and it takes exactly the
    // one argument the window server sends with it.
    unsafe {
        item.setTarget(Some(controls));
        item.setAction(Some(sel!(pressed:)));
    }

    item
}

/// Returns the system's drawing of a symbol, or `None` if this system has no such symbol.
fn symbol_image(name: &str) -> Option<Retained<NSImage>> {
    NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), None)
}

/// Returns the `NSWindow` behind an SDL window.
fn cocoa_window(window: &Window) -> Option<Retained<NSWindow>> {
    // SAFETY: the window is alive for the duration of the call, the property name is SDL's own
    // constant, and what comes back is either null or the window's `NSWindow`.
    let raw = unsafe {
        SDL_GetPointerProperty(
            SDL_GetWindowProperties(window.raw()),
            SDL_PROP_WINDOW_COCOA_WINDOW_POINTER,
            core::ptr::null_mut(),
        )
    };

    let handle = NonNull::new(raw.cast::<NSWindow>())?;

    // SAFETY: SDL hands back an unretained pointer to a window it owns and keeps alive for
    // longer than this process shows anything, and retaining it makes the reference this
    // holds its own.
    Some(unsafe { Retained::retain(handle.as_ptr())? })
}
