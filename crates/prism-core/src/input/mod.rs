//! Injecting the client's input on the host.
//!
//! This is the one part of the pipeline where latency is felt rather than seen. Video
//! arriving a few milliseconds late looks the same; a cursor that answers a few
//! milliseconds late feels wrong immediately. So input takes the shortest path there is:
//! no pacing, no batching, sent the instant it happens and injected the instant it lands.
//!
//! Each platform injects through its own API, and the differences run deeper than the
//! call names — macOS has to be granted permission and track its own pointer and modifier
//! state, Windows has neither problem but needs scan codes instead of key codes. The
//! [`Injector`] trait is what they have in common, and [`PlatformInjector`] resolves to
//! whichever one this build targets, so nothing above this module is written twice.

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;

/// Reason an input event could not be injected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InputError {
    /// The user has not granted permission to control the machine.
    ///
    /// On macOS this is the Accessibility entry in Privacy settings. Injection silently
    /// does nothing without it rather than failing, which is worse than an error, so the
    /// injector checks up front and says so.
    #[error("permission to control this machine has not been granted")]
    PermissionDenied,

    /// The platform refused to build or post an event.
    #[error("could not inject input: {reason}")]
    Inject {
        /// What went wrong.
        reason: &'static str,
    },

    /// The system accepted the call and inserted nothing.
    ///
    /// Windows reports refusal this way rather than through a permission check, so the
    /// code is the only thing that separates the causes. `5`, access denied, is User
    /// Interface Privilege Isolation refusing a process of lower integrity than the
    /// foreground window. A process with no interactive desktop at all — a service, or an
    /// SSH login — has nothing to inject into and fails here too.
    #[error("the system refused the event (Win32 error {code})")]
    Refused {
        /// The Win32 error code the failed call left behind.
        code: u32,
    },

    /// This build has no injector for the platform it is running on.
    #[error("input injection is not implemented on this platform yet")]
    Unsupported,
}

/// Puts input events onto the machine the host is running on.
///
/// Dispatch is static — [`PlatformInjector`] is a concrete type, not a trait object — so
/// the trait costs nothing at runtime. It exists to keep the platform implementations the
/// same shape as each other, which a type alias alone would not enforce.
pub trait Injector: Sized {
    /// Creates an injector for this machine.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::PermissionDenied`] if the platform requires permission to
    /// control the machine and it has not been granted, [`InputError::Unsupported`] on a
    /// platform with no implementation, and [`InputError::Inject`] if the platform will
    /// not set up whatever injection needs.
    fn new() -> Result<Self, InputError>;

    /// Injects one event.
    ///
    /// # Errors
    ///
    /// Returns [`InputError::Inject`] if the event cannot be built, which includes keys
    /// this build cannot map, and [`InputError::Refused`] if the system takes the call
    /// and does nothing with it.
    fn inject(&mut self, event: InputEvent) -> Result<(), InputError>;

    /// Releases every key and pointer button this injector is holding down.
    ///
    /// Injection is one-sided. The host presses what the client tells it to press and has
    /// no way to notice that the client has stopped talking about a key — so a modifier
    /// held when the client's window loses focus stays down on this machine for the rest
    /// of the session, and every keystroke after it becomes a shortcut. This is the way
    /// out: whoever drives the injector calls it the moment it can no longer be sure what
    /// the far end is holding.
    ///
    /// Only what was actually injected is released. A key-up for a key nobody pressed is
    /// still an event applications act on, so releasing indiscriminately would trade one
    /// wrong state for another.
    ///
    /// # Errors
    ///
    /// Returns the first failure, having attempted every release regardless. A modifier
    /// left down is the fault this exists to prevent, so one key the platform refuses does
    /// not get to strand the rest.
    fn release_all(&mut self) -> Result<(), InputError>;

    /// Returns whether injected events are reaching the system.
    ///
    /// Worth checking once, after a few events have gone out. Both platforms can accept
    /// an event and do nothing with it, for different reasons, and neither says so on the
    /// call that failed — so this is separate from [`Injector::inject`] rather than part
    /// of it. It answers about the recent past, not the last call.
    fn injection_is_landing(&self) -> bool;
}

/// How many HID usages [`HeldKeys`] has room for.
///
/// The keyboard usage page ends at `0xE7`, so a whole byte of usages covers it with room
/// to spare and the bounds check costs one comparison.
const HELD_KEY_CAPACITY: u16 = 256;

/// The keys an injector has pressed and not yet released.
///
/// A fixed bitset rather than a set that allocates. Input is the one path in the pipeline
/// where latency is felt rather than seen, and the first key press of a session is not the
/// place to be asking the allocator for anything.
///
/// Both platforms need this and neither needs it differently, so it lives here rather than
/// twice.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HeldKeys {
    words: [u64; (HELD_KEY_CAPACITY as usize).div_ceil(u64::BITS as usize)],
}

impl HeldKeys {
    /// Records a usage as held or released.
    ///
    /// A usage past the end of the page is dropped rather than refused. Nothing injects
    /// one — the platform tables have no entry for it — and this arrived over the network,
    /// so it is not worth a second error path beside the one the caller already has.
    pub fn set(&mut self, usage: u16, held: bool) {
        let Some((word, bit)) = Self::place(usage) else {
            return;
        };

        if held {
            self.words[word] |= 1 << bit;
        } else {
            self.words[word] &= !(1 << bit);
        }
    }

    /// Returns whether a usage is recorded as held.
    #[must_use]
    pub fn contains(&self, usage: u16) -> bool {
        Self::place(usage).is_some_and(|(word, bit)| self.words[word] & (1 << bit) != 0)
    }

    /// Returns how many keys are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.words
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    /// Returns whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|word| *word == 0)
    }

    /// Visits every held usage, lowest first.
    ///
    /// The order is the reason this is not a `HashSet` iteration: the modifiers sit at the
    /// end of the usage page, so ascending order releases the ordinary keys while their
    /// modifiers are still down, which is the order real hardware produces.
    pub fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        (0..HELD_KEY_CAPACITY).filter(|usage| self.contains(*usage))
    }

    /// Locates the word and bit a usage lives in, if it is on the page at all.
    fn place(usage: u16) -> Option<(usize, u32)> {
        if usage >= HELD_KEY_CAPACITY {
            return None;
        }

        Some((
            usize::from(usage) / u64::BITS as usize,
            u32::from(usage) % u64::BITS,
        ))
    }
}

use std::time::{Duration, Instant};

use crate::net::packet::InputEvent;

/// Where the pointer is on this machine, and how big the screen holding it is.
///
/// The screen travels with the position because the two are only meaningful together: the
/// client scales the position into its own window, and it cannot do that against a screen
/// size it has to remember from an earlier message that may never have arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerSample {
    /// Pixels from the left of the primary display.
    pub x: u16,
    /// Pixels from the top of the primary display.
    pub y: u16,
    /// Width of the primary display in pixels; never zero.
    pub screen_width: u16,
    /// Height of the primary display in pixels; never zero.
    pub screen_height: u16,
}

/// Reads where the pointer is on this machine.
///
/// Deliberately not a method on [`Injector`]. Reading the pointer has nothing to do with
/// injecting: the host samples it every frame from the thread that sends video, which owns
/// no injector, and the answer has to include movement the host's own user made rather than
/// only what this process injected.
///
/// Returns `None` on a platform with no implementation, and on any platform that will not
/// say — a session with no desktop has no pointer to report.
#[must_use]
pub fn pointer() -> Option<PointerSample> {
    #[cfg(target_os = "macos")]
    {
        macos::pointer()
    }

    #[cfg(target_os = "windows")]
    {
        windows::pointer()
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// The injector for the platform this build targets.
#[cfg(target_os = "macos")]
pub type PlatformInjector = macos::MacInjector;

/// The injector for the platform this build targets.
#[cfg(target_os = "windows")]
pub type PlatformInjector = windows::WindowsInjector;

/// The injector for the platform this build targets.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub type PlatformInjector = Unsupported;

/// Stands in for an injector on a platform that has none yet.
///
/// It fails at construction rather than accepting events and discarding them, so a host
/// started on such a platform says so once at startup instead of looking like it works.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub struct Unsupported;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl Injector for Unsupported {
    fn new() -> Result<Self, InputError> {
        Err(InputError::Unsupported)
    }

    fn inject(&mut self, _event: InputEvent) -> Result<(), InputError> {
        Err(InputError::Unsupported)
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        Ok(())
    }

    fn injection_is_landing(&self) -> bool {
        false
    }
}

/// How long the pointer stays with the person at this machine after they last moved it.
///
/// A second: long enough that somebody reaching for their own mouse is not fought for it by
/// the machine watching, short enough that the far side has it back as soon as they let go.
pub const LOCAL_HOLD: Duration = Duration::from_secs(1);

/// How far the pointer can be from where it was put and still count as where it was put.
///
/// In points on macOS and pixels on Windows. Putting it there rounds one way and reading it
/// back rounds another, so exactly there is not a test anything passes.
const LOCAL_SLACK: u16 = 3;

/// How many recent placements count as the far side's own.
///
/// More than one, because the system moves the pointer a moment after it is told to, and a
/// reading taken in between finds it where an earlier placement left it.
const RECENT_PLACEMENTS: usize = 8;

/// Who has the pointer: the person at this machine, or the machine watching it.
///
/// Nothing on either platform says which hand moved a pointer, and an injected event looks to
/// the system like any other. So this remembers where the far side's input put the pointer,
/// and a pointer found anywhere else — and somewhere it was not the last time it was looked at
/// either — was moved by somebody here. For [`LOCAL_HOLD`] after that, the far side's pointer
/// input is set aside, so the person sitting at the machine is never fought for their own mouse.
#[derive(Debug, Default)]
pub struct LocalHand {
    /// The last few places the far side put the pointer, as fractions of the screen.
    placed: [Option<(u16, u16)>; RECENT_PLACEMENTS],
    /// Where the next placement is written in the ring above.
    next: usize,
    /// Where the pointer was the last time it was looked at, on the screen.
    seen: Option<(u16, u16)>,
    /// Until when the pointer is the person's here, if they have taken it.
    until: Option<Instant>,
}

impl LocalHand {
    /// Records that the far side put the pointer a fraction of the way across and down the
    /// screen, on the scale the wire carries.
    pub fn placed(&mut self, x: u16, y: u16) {
        self.placed[self.next] = Some((x, y));
        self.next = (self.next + 1) % RECENT_PLACEMENTS;
    }

    /// Forgets where the pointer was put, after a move whose destination is not known.
    ///
    /// Relative motion lands wherever the pointer was, plus the distance. Until the next
    /// placement nothing here knows where that is, and guessing would be taking the far side's
    /// own movement for somebody else's.
    pub fn lost_track(&mut self) {
        self.placed = [None; RECENT_PLACEMENTS];
        self.seen = None;
    }

    /// Returns whether the person at this machine has the pointer, given where it is now.
    ///
    /// `now` is the pointer as [`pointer`] reads it, or `None` when it cannot be read, which
    /// changes nothing. The first look after starting, or after [`LocalHand::lost_track`], has
    /// nothing to compare against and never counts as somebody here.
    pub fn holds(&mut self, now: Option<PointerSample>, at: Instant) -> bool {
        if let Some(sample) = now {
            let here = (sample.x, sample.y);
            let first = self.seen.is_none() && self.placed.iter().all(Option::is_none);
            let put_there = self
                .placed
                .iter()
                .flatten()
                .any(|&(x, y)| near(here, on_screen(x, y, sample)));
            let left_there = self.seen.is_some_and(|seen| near(here, seen));

            if !first && !put_there && !left_there {
                self.until = Some(at + LOCAL_HOLD);
            }

            self.seen = Some(here);
        }

        self.until.is_some_and(|until| at < until)
    }
}

/// Returns whether two places on the screen are the same place, give or take [`LOCAL_SLACK`].
fn near(one: (u16, u16), other: (u16, u16)) -> bool {
    one.0.abs_diff(other.0) <= LOCAL_SLACK && one.1.abs_diff(other.1) <= LOCAL_SLACK
}

/// Returns where a fraction of the screen lands on the screen a sample was read from.
///
/// The last point is the last column and row, as it is where the injectors put it.
fn on_screen(x: u16, y: u16, screen: PointerSample) -> (u16, u16) {
    let scale = |fraction: u16, extent: u16| -> u16 {
        let last = u32::from(extent.saturating_sub(1));
        let whole = u32::from(u16::MAX);

        // At most `last`, which came from a `u16`.
        ((u32::from(fraction) * last + whole / 2) / whole) as u16
    };

    (
        scale(x, screen.screen_width),
        scale(y, screen.screen_height),
    )
}

#[cfg(test)]
mod local_hand {
    use std::time::{Duration, Instant};

    use super::{LOCAL_HOLD, LocalHand, PointerSample};

    /// The pointer at a place on a 1728 × 966 screen, the size the test machine reports.
    fn at(x: u16, y: u16) -> Option<PointerSample> {
        Some(PointerSample {
            x,
            y,
            screen_width: 1728,
            screen_height: 966,
        })
    }

    #[test]
    fn the_first_look_is_never_somebody_here() {
        let mut hand = LocalHand::default();

        assert!(!hand.holds(at(400, 300), Instant::now()));
    }

    #[test]
    fn a_pointer_where_the_far_side_put_it_is_the_far_sides() {
        let mut hand = LocalHand::default();
        let now = Instant::now();

        assert!(!hand.holds(at(10, 10), now));

        // Halfway across and down, which on this screen is 863.5 and 482.5.
        hand.placed(u16::MAX / 2, u16::MAX / 2);

        assert!(!hand.holds(at(863, 482), now));
    }

    #[test]
    fn a_pointer_moved_somewhere_else_is_the_persons_here_for_a_while() {
        let mut hand = LocalHand::default();
        let now = Instant::now();

        assert!(!hand.holds(at(10, 10), now));
        hand.placed(u16::MAX / 2, u16::MAX / 2);
        assert!(!hand.holds(at(863, 482), now));

        assert!(hand.holds(at(1200, 700), now), "moved by somebody here");
        assert!(
            hand.holds(at(1200, 700), now + LOCAL_HOLD - Duration::from_millis(1)),
            "and theirs until the hold runs out"
        );
        assert!(
            !hand.holds(at(1200, 700), now + LOCAL_HOLD),
            "then the far side's again, once they have stopped moving it"
        );
    }

    #[test]
    fn a_pointer_still_catching_up_with_an_earlier_placement_is_not_somebody_here() {
        let mut hand = LocalHand::default();
        let now = Instant::now();

        assert!(!hand.holds(at(0, 0), now));
        hand.placed(0, 0);
        hand.placed(u16::MAX / 4, u16::MAX / 4);
        hand.placed(u16::MAX / 2, u16::MAX / 2);

        assert!(
            !hand.holds(at(432, 241), now),
            "where the placement before last put it"
        );
    }

    #[test]
    fn after_a_move_of_unknown_length_the_next_look_starts_over() {
        let mut hand = LocalHand::default();
        let now = Instant::now();

        assert!(!hand.holds(at(10, 10), now));
        hand.lost_track();

        assert!(!hand.holds(at(900, 600), now));
    }
}

#[cfg(test)]
mod tests {
    use super::HeldKeys;

    #[test]
    fn a_fresh_set_holds_nothing() {
        let keys = HeldKeys::default();

        assert!(keys.is_empty());
        assert_eq!(keys.len(), 0);
        assert_eq!(keys.iter().count(), 0);
    }

    #[test]
    fn a_key_is_held_until_it_is_released() {
        let mut keys = HeldKeys::default();

        keys.set(0xE1, true);
        assert!(keys.contains(0xE1), "left shift is down");
        assert!(!keys.contains(0xE5), "right shift is a different key");
        assert_eq!(keys.len(), 1);

        keys.set(0xE1, false);
        assert!(keys.is_empty(), "and up again");
    }

    #[test]
    fn releasing_every_held_key_empties_the_set() {
        let mut keys = HeldKeys::default();

        for usage in [0x04, 0x2C, 0x63, 0xE1, 0xE7] {
            keys.set(usage, true);
        }
        assert_eq!(keys.len(), 5);

        for usage in keys.iter().collect::<Vec<_>>() {
            keys.set(usage, false);
        }

        assert!(
            keys.is_empty(),
            "a set that still holds something after a release is a stuck modifier"
        );
    }

    #[test]
    fn the_modifiers_come_last() {
        // The release path depends on this order: an ordinary key has to go up while the
        // modifier it was pressed under is still down, the way real hardware reports it.
        let mut keys = HeldKeys::default();

        keys.set(0xE2, true);
        keys.set(0x2B, true);
        keys.set(0x04, true);

        assert_eq!(keys.iter().collect::<Vec<_>>(), vec![0x04, 0x2B, 0xE2]);
    }

    #[test]
    fn a_usage_past_the_end_of_the_page_is_dropped_rather_than_recorded() {
        let mut keys = HeldKeys::default();

        keys.set(0xFFFF, true);

        assert!(keys.is_empty(), "nothing injects a usage that large");
        assert!(!keys.contains(0xFFFF));
    }
}
