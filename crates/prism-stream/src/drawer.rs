//! A handle at the left edge that opens a column of controls beside it.
//!
//! Two places need one. A stream window filling the screen has no title bar left to keep its
//! controls in, and a machine being watched needs somewhere to end it from that is always there
//! and never in the way. Both get the same thing: a small round handle at the edge, and beside
//! it, once pressed, the controls.
//!
//! Drawn by AppKit, for the reason the toolbar is. A control rasterised over the video would
//! need its own hit testing, its own hover and its own idea of what a button looks like; these
//! are the system's buttons, and a press arrives as a message.
//!
//! # Threading
//!
//! Main thread only, like every view. Whoever holds a [`Drawer`] is already there: the stream
//! window's loop, or a closure the shell runs on its main thread.

#![cfg(target_os = "macos")]

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSAutoresizingMaskOptions,
    NSBox, NSBoxType, NSButton, NSCellImagePosition, NSColor, NSFont, NSImage, NSTextAlignment,
    NSTextField, NSTitlePosition, NSView,
};
use objc2_foundation::{
    MainThreadMarker, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

/// How wide across the handle is.
const HANDLE: f64 = 28.0;

/// How far the handle and the column stand off the edge, and off each other.
const MARGIN: f64 = 10.0;

/// How wide the column of controls is.
const COLUMN: f64 = 230.0;

/// How tall one control in the column is.
const ROW: f64 = 34.0;

/// The space inside the column around what it holds.
const PAD: f64 = 12.0;

/// How tall the line at the top of the column is, when it has one.
const HEADING: f64 = 30.0;

/// Where along the left edge the drawer sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    /// Halfway up, where a handle that is always there is easiest to find.
    Middle,
    /// At the top, where a handle that appears when the pointer reaches the top edge is found.
    Top,
}

/// One control in the column.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The system symbol drawn beside its name.
    pub symbol: &'static str,
    /// What it is called.
    pub label: String,
}

/// The column of controls.
struct Column {
    /// The dark panel everything else sits on.
    panel: Retained<NSBox>,
    /// The line at the top, where there is one.
    heading: Option<Retained<NSTextField>>,
    /// One button an entry, in order.
    buttons: Vec<Retained<NSButton>>,
}

/// The views, held by the object their presses are sent to.
struct Parts {
    /// Told which entry was pressed, by its place in the list.
    on_press: Box<dyn Fn(usize)>,
    /// The round handle, and the button filling it.
    knob: RefCell<Option<(Retained<NSBox>, Retained<NSButton>)>>,
    /// The column, once it is built.
    column: RefCell<Option<Column>>,
    /// Whether the column is showing.
    open: Cell<bool>,
    /// Whether the handle is showing at all.
    shown: Cell<bool>,
    /// Whether the window holding this is sized to it, rather than it being laid over a window.
    fits_window: bool,
    /// Where along the edge it sits.
    anchor: Anchor,
}

define_class!(
    // SAFETY:
    // - `NSObject` has no subclassing requirements.
    // - `Target` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Parts]
    struct Target;

    unsafe impl NSObjectProtocol for Target {}

    impl Target {
        /// Opens the column, or closes it.
        #[unsafe(method(toggled:))]
        fn toggled(&self, _sender: &NSButton) {
            self.set_open(!self.ivars().open.get());
        }

        /// Passes on which entry was pressed, and closes the column behind it.
        ///
        /// Closed first, so that whatever the press does — leave full screen, end the session —
        /// happens to a drawer that is already out of the way.
        #[unsafe(method(pressed:))]
        fn pressed(&self, sender: &NSButton) {
            self.set_open(false);

            if let Ok(index) = usize::try_from(sender.tag()) {
                (self.ivars().on_press)(index);
            }
        }
    }
);

impl Target {
    /// Shows or hides the column, and puts the handle where it belongs beside it.
    fn set_open(&self, open: bool) {
        self.ivars().open.set(open);
        self.lay_out();
    }

    /// Puts every view where the current state says it goes.
    ///
    /// In the coordinates of whatever holds them, which count up from the bottom unless that
    /// view says it is flipped. Each view is told to stay where it is put relative to the edge
    /// it is anchored to when the holder changes size — a window going full screen moves the
    /// middle and the top, and a handle left where either used to be is in the wrong place.
    fn lay_out(&self) {
        let parts = self.ivars();
        let open = parts.open.get() && parts.shown.get();
        let height = self.column_height();

        if parts.fits_window {
            self.fit_window(open, height);
        }

        let knob = parts.knob.borrow();
        let column = parts.column.borrow();

        let Some((knob, button)) = knob.as_ref() else {
            return;
        };
        // SAFETY: read on the main thread, of a view this put into its holder itself; what
        // comes back is that holder, retained for the length of the layout.
        let Some(holder) = (unsafe { knob.superview() }) else {
            return;
        };

        let place = Placing {
            height: holder.bounds().size.height,
            flipped: holder.isFlipped(),
            anchor: parts.anchor,
        };
        let (across, arrow) = if open {
            (MARGIN + COLUMN + MARGIN, "chevron.left")
        } else {
            (MARGIN, "chevron.right")
        };

        knob.setFrame(rect(across, place.bottom_of(HANDLE), HANDLE, HANDLE));
        knob.setAutoresizingMask(place.staying());
        knob.setHidden(!parts.shown.get());
        button.setImage(symbol(arrow).as_deref());

        if let Some(column) = column.as_ref() {
            column
                .panel
                .setFrame(rect(MARGIN, place.bottom_of(height), COLUMN, height));
            column.panel.setAutoresizingMask(place.staying());
            column.panel.setHidden(!open);
        }
    }

    /// Sizes the window holding the drawer to what it is showing.
    ///
    /// Its left edge and its middle stay where they are, so the handle does not move under the
    /// pointer that just pressed it. A window that stayed its open size with the column hidden
    /// would be an invisible rectangle taking clicks meant for whatever is under it.
    fn fit_window(&self, open: bool, column_height: f64) {
        let knob = self.ivars().knob.borrow();

        let Some(window) = knob.as_ref().and_then(|(knob, _)| knob.window()) else {
            return;
        };

        let (width, height) = if open {
            (
                MARGIN + COLUMN + MARGIN + HANDLE + MARGIN,
                column_height.max(HANDLE) + 2.0 * MARGIN,
            )
        } else {
            (MARGIN + HANDLE + MARGIN, HANDLE + 2.0 * MARGIN)
        };

        let was = window.frame();
        let middle = was.origin.y + was.size.height / 2.0;

        window.setFrame_display(
            rect(was.origin.x, middle - height / 2.0, width, height),
            true,
        );
    }

    /// How tall the column is, for the entries it holds.
    fn column_height(&self) -> f64 {
        self.ivars().column.borrow().as_ref().map_or(0.0, |column| {
            height_for(column.heading.is_some(), column.buttons.len())
        })
    }
}

/// Where along the holder's height a view of some height goes, for one anchor.
struct Placing {
    /// How tall the holder is.
    height: f64,
    /// Whether the holder counts down from the top rather than up from the bottom.
    flipped: bool,
    /// Which edge, or the middle, the views are placed against.
    anchor: Anchor,
}

impl Placing {
    /// Returns the coordinate of the edge a view of `tall` sits on, in the holder's terms.
    ///
    /// Its bottom in a holder that counts up and its top in one that counts down, which is
    /// the coordinate a frame's origin names in each.
    fn bottom_of(&self, tall: f64) -> f64 {
        match (self.anchor, self.flipped) {
            (Anchor::Middle, _) => (self.height - tall) / 2.0,
            (Anchor::Top, true) => MARGIN,
            (Anchor::Top, false) => self.height - MARGIN - tall,
        }
    }

    /// Returns which margins stretch when the holder changes size, so the view stays put.
    fn staying(&self) -> NSAutoresizingMaskOptions {
        match (self.anchor, self.flipped) {
            (Anchor::Middle, _) => {
                NSAutoresizingMaskOptions::ViewMinYMargin
                    | NSAutoresizingMaskOptions::ViewMaxYMargin
            }
            // The margin between it and the bottom of the holder, whichever way that counts.
            (Anchor::Top, true) => NSAutoresizingMaskOptions::ViewMaxYMargin,
            (Anchor::Top, false) => NSAutoresizingMaskOptions::ViewMinYMargin,
        }
    }
}

/// A handle at the left edge of a view, and the column of controls it opens.
pub struct Drawer {
    target: Retained<Target>,
}

impl Drawer {
    /// Puts a drawer into `holder`, hidden until [`Drawer::set_shown`] says otherwise.
    ///
    /// `fits_window` is for a window that exists to hold the drawer and nothing else: that
    /// window is resized to what the drawer shows. A drawer laid over a window with something
    /// else in it leaves the window alone.
    ///
    /// `on_press` is told which entry was pressed, by its place in `entries`.
    ///
    /// Returns `None` off the main thread, where no view can be made.
    pub fn install(
        holder: &NSView,
        heading: Option<&str>,
        entries: &[Entry],
        fits_window: bool,
        anchor: Anchor,
        on_press: impl Fn(usize) + 'static,
    ) -> Option<Self> {
        let marker = MainThreadMarker::new()?;

        let target = Target::alloc(marker).set_ivars(Parts {
            on_press: Box::new(on_press),
            knob: RefCell::new(None),
            column: RefCell::new(None),
            open: Cell::new(false),
            shown: Cell::new(false),
            fits_window,
            anchor,
        });
        // SAFETY: `init` on `NSObject` takes no arguments and returns the object it was sent
        // to, and the instance variables it needs were set on the allocation above.
        let target: Retained<Target> = unsafe { msg_send![super(target), init] };

        let column = column(&target, heading, entries, marker);
        let knob = knob(&target, marker);

        holder.addSubview(&column.panel);
        holder.addSubview(&knob.0);

        target.ivars().column.replace(Some(column));
        target.ivars().knob.replace(Some(knob));
        target.lay_out();

        Some(Self { target })
    }

    /// Shows the handle, or hides it and the column with it.
    pub fn set_shown(&self, shown: bool) {
        let parts = self.target.ivars();

        parts.shown.set(shown);

        if !shown {
            parts.open.set(false);
        }

        self.target.lay_out();
    }

    /// Closes the column, leaving the handle.
    pub fn close(&self) {
        self.target.set_open(false);
    }

    /// Returns whether the handle is showing.
    #[must_use]
    pub fn is_shown(&self) -> bool {
        self.target.ivars().shown.get()
    }

    /// Returns whether the column is open beside the handle.
    #[must_use]
    pub fn is_open(&self) -> bool {
        let parts = self.target.ivars();

        parts.shown.get() && parts.open.get()
    }

    /// Changes the line at the top of the column, where there is one.
    pub fn set_heading(&self, heading: &str) {
        if let Some(line) = self
            .target
            .ivars()
            .column
            .borrow()
            .as_ref()
            .and_then(|column| column.heading.as_ref())
        {
            line.setStringValue(&NSString::from_str(heading));
        }
    }

    /// Changes what one entry says and shows.
    pub fn set_entry(&self, index: usize, entry: &Entry) {
        if let Some(button) = self
            .target
            .ivars()
            .column
            .borrow()
            .as_ref()
            .and_then(|column| column.buttons.get(index))
        {
            button.setTitle(&NSString::from_str(&entry.label));
            button.setImage(symbol(entry.symbol).as_deref());
        }
    }

    /// Returns whether a place is over the handle or the open column.
    ///
    /// Measured from the holder's top left corner, as a pointer is, whichever way the holder
    /// counts. For a window whose clicks would otherwise go somewhere: a stream window sends the
    /// ones over its picture to the far machine, and the ones over its own controls must not.
    #[must_use]
    pub fn covers(&self, x: f64, from_top: f64) -> bool {
        let parts = self.target.ivars();

        if !parts.shown.get() {
            return false;
        }

        let knob = parts.knob.borrow();
        // SAFETY: read on the main thread, of a view this put into its holder itself.
        let Some(holder) = knob
            .as_ref()
            .and_then(|(knob, _)| unsafe { knob.superview() })
        else {
            return false;
        };

        let point = if holder.isFlipped() {
            NSPoint::new(x, from_top)
        } else {
            NSPoint::new(x, holder.bounds().size.height - from_top)
        };
        drop(knob);

        let over = |frame: NSRect| {
            point.x >= frame.origin.x
                && point.x <= frame.origin.x + frame.size.width
                && point.y >= frame.origin.y
                && point.y <= frame.origin.y + frame.size.height
        };

        let knob = parts
            .knob
            .borrow()
            .as_ref()
            .is_some_and(|(knob, _)| over(knob.frame()));
        let column = parts.open.get()
            && parts
                .column
                .borrow()
                .as_ref()
                .is_some_and(|column| over(column.panel.frame()));

        knob || column
    }
}

impl core::fmt::Debug for Drawer {
    /// Says whether it is showing, without reaching into the views.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let parts = self.target.ivars();

        f.debug_struct("Drawer")
            .field("shown", &parts.shown.get())
            .field("open", &parts.open.get())
            .finish_non_exhaustive()
    }
}

/// Builds the round handle: a blue disc with a chevron on it.
fn knob(target: &Target, marker: MainThreadMarker) -> (Retained<NSBox>, Retained<NSButton>) {
    let disc = plain_box(marker, &NSColor::systemBlueColor(), HANDLE / 2.0);

    let arrow = symbol("chevron.right").unwrap_or_default();
    // SAFETY: the target is not retained, which is the usual Objective-C rule; what keeps it
    // alive is the `Drawer` holding it for as long as these views exist. The selector is one
    // the class defines, taking the single sender argument a button sends.
    let button = unsafe {
        NSButton::buttonWithImage_target_action(
            &arrow,
            Some(as_object(target)),
            Some(sel!(toggled:)),
            marker,
        )
    };

    button.setBordered(false);
    button.setContentTintColor(Some(&NSColor::whiteColor()));
    button.setFrame(rect(0.0, 0.0, HANDLE, HANDLE));

    if let Some(inside) = disc.contentView() {
        inside.addSubview(&button);
    }

    (disc, button)
}

/// How tall a column is with this many entries, and a heading or not.
fn height_for(heading: bool, entries: usize) -> f64 {
    PAD + if heading { HEADING } else { 0.0 } + ROW * entries as f64 + PAD
}

/// Builds the column: a dark panel with an optional line at the top and one button a row.
fn column(
    target: &Target,
    heading: Option<&str>,
    entries: &[Entry],
    marker: MainThreadMarker,
) -> Column {
    let panel = plain_box(marker, &NSColor::colorWithWhite_alpha(0.1, 0.94), 12.0);
    let height = height_for(heading.is_some(), entries.len());

    panel.setFrame(rect(0.0, 0.0, COLUMN, height));
    panel.setBorderWidth(1.0);
    panel.setBorderColor(&NSColor::colorWithWhite_alpha(1.0, 0.1));

    // Dark whatever the machine is set to, because the panel is: light text on it has to stay
    // light in a light appearance too.
    // SAFETY: a constant the framework defines, read once it has been linked.
    let dark = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua });
    panel.setAppearance(dark.as_deref());

    let Some(inside) = panel.contentView() else {
        return Column {
            panel,
            heading: None,
            buttons: Vec::new(),
        };
    };

    let mut top = height - PAD;

    let line = heading.map(|heading| {
        let line = NSTextField::labelWithString(&NSString::from_str(heading), marker);

        line.setTextColor(Some(&NSColor::secondaryLabelColor()));
        line.setFont(Some(&NSFont::systemFontOfSize(12.0)));
        line.setFrame(rect(
            PAD,
            top - HEADING + 6.0,
            COLUMN - 2.0 * PAD,
            HEADING - 8.0,
        ));
        inside.addSubview(&line);

        top -= HEADING;
        line
    });

    let buttons = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let button = entry_button(target, entry, marker);

            button.setTag(isize::try_from(index).unwrap_or(isize::MAX));
            button.setFrame(rect(
                PAD - 4.0,
                top - ROW * (index + 1) as f64,
                COLUMN - 2.0 * PAD,
                ROW,
            ));
            inside.addSubview(&button);

            button
        })
        .collect();

    Column {
        panel,
        heading: line,
        buttons,
    }
}

/// Builds one entry's button: its symbol, then its name, left aligned and without a border.
fn entry_button(target: &Target, entry: &Entry, marker: MainThreadMarker) -> Retained<NSButton> {
    let title = NSString::from_str(&entry.label);

    // SAFETY: as for the handle — the target outlives the button, and the selector is one the
    // class defines, taking the single sender argument a button sends.
    let button = unsafe {
        match symbol(entry.symbol) {
            Some(image) => NSButton::buttonWithTitle_image_target_action(
                &title,
                &image,
                Some(as_object(target)),
                Some(sel!(pressed:)),
                marker,
            ),
            None => NSButton::buttonWithTitle_target_action(
                &title,
                Some(as_object(target)),
                Some(sel!(pressed:)),
                marker,
            ),
        }
    };

    button.setBordered(false);
    button.setImagePosition(NSCellImagePosition::ImageLeading);
    button.setAlignment(NSTextAlignment::Left);
    button.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    button.setContentTintColor(Some(&NSColor::whiteColor()));

    button
}

/// Builds a box that is only a filled, rounded shape, with no title and no border inset.
fn plain_box(marker: MainThreadMarker, fill: &NSColor, radius: f64) -> Retained<NSBox> {
    let shape = NSBox::initWithFrame(NSBox::alloc(marker), rect(0.0, 0.0, HANDLE, HANDLE));

    shape.setBoxType(NSBoxType::Custom);
    shape.setTitlePosition(NSTitlePosition::NoTitle);
    shape.setContentViewMargins(NSSize::new(0.0, 0.0));
    shape.setBorderWidth(0.0);
    shape.setCornerRadius(radius);
    shape.setFillColor(fill);

    shape
}

/// Returns the system's drawing of a symbol, or `None` if this system has no such symbol.
fn symbol(name: &str) -> Option<Retained<NSImage>> {
    NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), None)
}

/// Returns the target as the untyped object a button's target is.
fn as_object(target: &Target) -> &AnyObject {
    target
}

/// Builds a rectangle from its corner and size.
fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}
