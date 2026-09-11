//! Showing the decoded stream in a window.
//!
//! The window and the event loop live on the main thread because macOS requires it, and
//! presentation happens there too. Receiving and decoding run on their own threads, which
//! is the three-thread split the client is meant to have: one thread that only reads the
//! socket, one that only decodes, and one that only draws.
//!
//! Pictures reach this thread through a two-deep channel and are dropped rather than
//! queued when it is full. A picture that waits its turn is already too late to be worth
//! showing.
//!
//! # What differs between the two clients
//!
//! Only the surface. SDL opens the window on both, the event loop and the pacing are the same
//! code, and what changes underneath is how a decoded picture reaches the screen: a
//! `CAMetalLayer` and a Metal command buffer on macOS, a flip-model swap chain and a Direct3D
//! draw on Windows. That is what [`surface`] is — the same three operations, twice.

use std::error::Error;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use prism_core::cursor::CursorTracker;
use prism_core::net::packet::{InputEvent, MouseButton};
use prism_core::net::transfer::{self, Files, Landed};
use prism_core::render::pacing::PresentPacer;
use prism_core::render::{Fitted, fit};
use prism_core::stats::LatencyRecorder;
use sdl3::event::Event;
use sdl3::keyboard::{Keycode, Mod};
use sdl3::mouse::MouseButton as SdlMouseButton;

use prism_core::control::client::{self, ClientConfig, Reporter};

#[cfg(target_os = "macos")]
#[path = "display/metal.rs"]
mod surface;
#[cfg(target_os = "windows")]
#[path = "display/d3d11.rs"]
mod surface;
#[cfg(target_os = "macos")]
#[path = "display/toolbar.rs"]
mod toolbar;
#[cfg(not(target_os = "macos"))]
#[path = "display/toolbar_none.rs"]
mod toolbar;

/// How many pictures may wait to be shown before the newest is dropped.
const PICTURE_QUEUE_DEPTH: usize = 2;

/// How often the statistics overlay is redrawn.
///
/// Ten times a second. Faster would be unreadable and would put CPU text rasterisation on
/// a path that exists to avoid exactly that kind of work.
const HUD_INTERVAL: Duration = Duration::from_millis(100);

/// How wide the statistics panel is, in pixels.
const HUD_WIDTH: usize = 340;

/// How tall the statistics panel is, in pixels.
///
/// Room for the four numbers, the line saying whether the far machine is being controlled, and
/// a line for each direction a file may be moving in.
const HUD_HEIGHT: usize = 184;

/// How large the statistics panel's text is, in pixels.
const HUD_FONT_SIZE: f64 = 13.0;

/// The statistics panel's width, height and text size for a window with this many pixels to
/// a point.
///
/// The three above are what the panel measures in points. It is drawn in pixels, so on a
/// Retina window it would otherwise come out at half the size with text too small to read.
fn hud_measure(scale: f64) -> (usize, usize, f64) {
    let scale = scale.max(1.0);

    (
        (HUD_WIDTH as f64 * scale).round() as usize,
        (HUD_HEIGHT as f64 * scale).round() as usize,
        HUD_FONT_SIZE * scale,
    )
}

/// Returns whether an event should end the session.
///
/// Escape cannot be the way out once input is being forwarded — the host needs it. The
/// combination below is deliberately awkward so that nothing a game or an application
/// wants can trigger it by accident.
fn is_quit(event: &Event) -> bool {
    match event {
        Event::Quit { .. } => true,
        Event::KeyDown {
            keycode: Some(Keycode::Q),
            keymod,
            ..
        } => {
            keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD)
                && keymod.intersects(Mod::LALTMOD | Mod::RALTMOD)
                && keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD)
        }
        _ => false,
    }
}

/// Returns whether an event is the chord that stops controlling the far machine, or starts again.
///
/// Control and option together, pressed with nothing else. Shift is excluded so that reaching
/// for the quit chord does not change anything on the way.
///
/// Read from the modifiers rather than from the key, so it does not matter which of the two
/// went down first or whether they are the left or the right one.
fn is_control_toggle(event: &Event) -> bool {
    let Event::KeyDown {
        keycode: Some(key),
        keymod,
        repeat: false,
        ..
    } = event
    else {
        return false;
    };

    if !matches!(
        key,
        Keycode::LCtrl | Keycode::RCtrl | Keycode::LAlt | Keycode::RAlt
    ) {
        return false;
    }

    keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD)
        && keymod.intersects(Mod::LALTMOD | Mod::RALTMOD)
        && !keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD)
}

/// Returns whether an event is a key or a button being let go.
///
/// Sent whether or not this machine is controlling the far one, because the chord that stops
/// it is two keys held down, and a drag can be let go of after it stopped: the host saw them
/// pressed and would go on holding them if the release stayed on this machine.
fn is_release(event: &Event) -> bool {
    matches!(event, Event::KeyUp { .. } | Event::MouseButtonUp { .. })
}

/// Returns where on the far screen a place in the window is, as the wire carries it.
///
/// Measured against the picture rather than the window, because the picture keeps its shape
/// and the window need not: a place on the bars beside it is the nearest place on the far
/// screen's edge. The last point of the picture is the last point of the screen.
///
/// # Examples
///
/// ```ignore
/// let shown = Fitted::whole((1280.0, 720.0));
/// assert_eq!(to_fraction(0.0, 0.0, shown), (0, 0));
/// assert_eq!(to_fraction(1279.0, 719.0, shown), (65535, 65535));
/// ```
fn to_fraction(x: f32, y: f32, shown: Fitted) -> (u16, u16) {
    let (across, down) = shown.to_picture(x, y);

    (
        (across * f32::from(u16::MAX)).round() as u16,
        (down * f32::from(u16::MAX)).round() as u16,
    )
}

/// Returns where the picture is in the window, in the window's own units.
///
/// The whole window until a picture has arrived, when there is nothing yet to keep the shape
/// of — the pointer still goes somewhere sensible in the moment before the first one.
fn shown_in(picture: Option<(u32, u32)>, area: (u32, u32)) -> Fitted {
    let area = (area.0 as f32, area.1 as f32);

    picture.map_or_else(|| Fitted::whole(area), |picture| fit(picture, area))
}

/// Returns the size of the largest screen attached, in pixels, or `None` if none will say.
///
/// Largest by area, and in pixels rather than points: a Retina screen of 1728 points across has
/// 3456 pixels to fill, and it is pixels the far machine is being asked to send.
fn largest_screen(video: &sdl3::VideoSubsystem) -> Option<(u32, u32)> {
    video
        .displays()
        .ok()?
        .iter()
        .filter_map(|display| display.get_mode().ok())
        .map(|mode| {
            let density = mode.pixel_density.max(1.0);

            (
                (mode.w.max(0) as f32 * density).round() as u32,
                (mode.h.max(0) as f32 * density).round() as u32,
            )
        })
        .max_by_key(|(across, down)| u64::from(*across) * u64::from(*down))
}

/// Returns where in the window a pointer event happened, when the event is one that has a place.
fn pointer_at(event: &Event) -> Option<(f32, f32)> {
    match event {
        Event::MouseMotion { x, y, .. }
        | Event::MouseButtonDown { x, y, .. }
        | Event::MouseButtonUp { x, y, .. } => Some((*x, *y)),
        _ => None,
    }
}

/// Returns where a click was made, when the event is one.
///
/// Sent ahead of the button itself. The host's pointer is wherever the last motion put it,
/// and a click that arrives with no motion before it — the first after control was taken, or
/// one made without moving — would otherwise land there rather than where it was made.
fn where_clicked(event: &Event, shown: Fitted) -> Option<InputEvent> {
    match event {
        Event::MouseButtonDown { x, y, .. } | Event::MouseButtonUp { x, y, .. } => {
            let (x, y) = to_fraction(*x, *y, shown);

            Some(InputEvent::MouseTo { x, y })
        }
        _ => None,
    }
}

/// Translates one SDL event into an input event for the host, if it is one.
///
/// Pointer motion goes as a place rather than a distance: the pointer here is over a picture
/// of the far screen, and where it points is where the far pointer belongs. Sending how far it
/// moved instead leaves the two pointers wherever they each happened to start, and they never
/// meet.
///
/// Key repeats are dropped. The host's own operating system generates repeats from the
/// key being held, so forwarding the client's as well would double them.
fn to_input_event(event: &Event, shown: Fitted) -> Option<InputEvent> {
    match event {
        Event::MouseMotion { x, y, .. } => {
            let (x, y) = to_fraction(*x, *y, shown);

            Some(InputEvent::MouseTo { x, y })
        }
        Event::MouseButtonDown { mouse_btn, .. } => Some(InputEvent::MouseButton {
            button: to_button(*mouse_btn)?,
            pressed: true,
        }),
        Event::MouseButtonUp { mouse_btn, .. } => Some(InputEvent::MouseButton {
            button: to_button(*mouse_btn)?,
            pressed: false,
        }),
        Event::MouseWheel {
            integer_x,
            integer_y,
            ..
        } => Some(InputEvent::MouseScroll {
            dx: *integer_x as i16,
            // SDL counts a wheel pushed away from the user as positive; the wire counts
            // downward as positive, to agree with pointer motion. Without this the host
            // scrolls the wrong way.
            dy: -(*integer_y as i16),
        }),
        Event::KeyDown {
            scancode: Some(scancode),
            repeat: false,
            ..
        } => Some(InputEvent::Key {
            usage: *scancode as u16,
            pressed: true,
        }),
        Event::KeyUp {
            scancode: Some(scancode),
            ..
        } => Some(InputEvent::Key {
            usage: *scancode as u16,
            pressed: false,
        }),
        _ => None,
    }
}

/// Feeds pointer motion to the tracker so the cursor moves without waiting for the host.
///
/// Only motion. A button or a key changes nothing about where the pointer is, and a scroll
/// moves the content rather than the cursor.
fn predict(cursor: &mut CursorTracker, stamped_ts_us: u64, event: InputEvent) {
    if let InputEvent::MouseMove { dx, dy } = event {
        cursor.moved(stamped_ts_us, dx, dy);
    }
}

/// Maps an SDL pointer button onto the wire's three.
///
/// Buttons beyond the first three are dropped rather than guessed at: they mean different
/// things on different mice and a wrong button is worse than none.
fn to_button(button: SdlMouseButton) -> Option<MouseButton> {
    match button {
        SdlMouseButton::Left => Some(MouseButton::Left),
        SdlMouseButton::Right => Some(MouseButton::Right),
        SdlMouseButton::Middle => Some(MouseButton::Middle),
        _ => None,
    }
}

/// What the overlay says about the session, gathered where each number is counted.
struct Showing {
    /// Pictures a second, over the last overlay interval.
    fps: f64,
    /// Pictures drawn since the session opened.
    shown: u64,
    /// Pictures dropped to stay in time.
    missed: u64,
    /// How far this machine's clock is from the host's, in microseconds.
    clock_offset_us: i64,
    /// Whether the far machine is being controlled, or `None` where it cannot be.
    control: Option<bool>,
}

/// Builds the lines the overlay shows.
///
/// Latency first, because it is what every milestone is judged on, and the pacing line
/// directly beneath it because that delay is part of the number above and should not have
/// to be remembered separately.
fn hud_lines(
    latency: &mut LatencyRecorder,
    pacer: &mut PresentPacer,
    session: &Showing,
    moving: &[transfer::Progress],
) -> Vec<String> {
    let &Showing {
        fps,
        shown,
        missed,
        clock_offset_us,
        control,
    } = session;
    let mut lines = Vec::with_capacity(8);

    match latency.summarize() {
        Some(summary) => lines.push(format!(
            "latency  p50 {:.1}  p95 {:.1}  p99 {:.1} ms",
            f64::from(summary.p50_us) / 1000.0,
            f64::from(summary.p95_us) / 1000.0,
            f64::from(summary.p99_us) / 1000.0,
        )),
        None => lines.push("latency  waiting".to_owned()),
    }

    if pacer.enabled() {
        lines.push(format!(
            "pacing   +{:.1} ms target, {} late",
            f64::from(pacer.delay_us()) / 1000.0,
            pacer.shown_late(),
        ));
    } else {
        lines.push("pacing   off".to_owned());
    }

    lines.push(format!(
        "display  {fps:.1} fps, {shown} shown, {missed} missed"
    ));

    if clock_offset_us == client::OFFSET_UNKNOWN {
        lines.push("clock    not synchronised".to_owned());
    } else {
        lines.push(format!(
            "clock    host {:+.2} ms",
            clock_offset_us as f64 / 1000.0
        ));
    }

    // Last, and only where there is something to say: a session that is watching and nothing
    // else has no chord to be told about.
    match control {
        Some(true) => lines.push("control  on, control option to stop".to_owned()),
        Some(false) => lines.push("control  off, control option to take it".to_owned()),
        None => {}
    }

    // A file is the one thing here somebody started by hand, so it is the one thing that has
    // to say it is happening. Without this, choosing a file and watching nothing change is
    // indistinguishable from a button that does not work.
    for one in moving {
        let share = if one.size == 0 {
            100.0
        } else {
            one.moved as f64 * 100.0 / one.size as f64
        };

        lines.push(format!(
            "{}  {} {}",
            if one.sending { "sending" } else { "getting" },
            one.name,
            if one.done {
                "done".to_owned()
            } else {
                format!("{share:.0}%")
            },
        ));
    }

    lines
}

/// Reports what the pacer cost and what it bought.
///
/// The delay it added belongs next to the end-to-end latency rather than hidden inside
/// it: smoothness is bought with latency, and the price should be visible.
fn report_pacing(say: &Reporter, pacer: &mut PresentPacer) {
    if !pacer.enabled() {
        say.note(format!(
            "pacing : off — every picture was shown the moment it decoded ({} pictures)",
            pacer.total()
        ));
        return;
    }

    let target = pacer.delay_us();
    let late = pacer.shown_late();
    let total = pacer.total();

    let Some(held) = pacer.held_summary() else {
        say.note("pacing : no pictures were paced");
        return;
    };

    say.note(format!(
        "pacing : target {:.2} ms, held p50 {:.2} p99 {:.2} max {:.2} ms, {late}/{total} arrived late",
        f64::from(target) / 1000.0,
        f64::from(held.p50_us) / 1000.0,
        f64::from(held.p99_us) / 1000.0,
        f64::from(held.max_us) / 1000.0,
    ));
}

/// Returns the new size in pixels when an event says the window changed.
fn resized(event: &Event) -> bool {
    matches!(
        event,
        Event::Window {
            win_event: sdl3::event::WindowEvent::PixelSizeChanged(..)
                | sdl3::event::WindowEvent::Resized(..),
            ..
        }
    )
}

/// Opens the system's file chooser and sends what was picked down `chosen`.
///
/// The dialog answers on SDL's event pump, which is this thread, so the callback cannot do the
/// work itself: it is running inside the poll the main loop is in the middle of. It hands the
/// path over and the loop picks it up on its next turn.
fn choose_a_file(
    window: &sdl3::video::Window,
    chosen: &std::sync::mpsc::Sender<std::path::PathBuf>,
    say: &Reporter,
) {
    let chosen = chosen.clone();
    let opened = sdl3::dialog::show_open_file_dialog(
        &[],
        None::<&std::path::Path>,
        false,
        window,
        Box::new(move |picked, _| {
            if let Ok(paths) = picked {
                if let Some(path) = paths.into_iter().next() {
                    let _ = chosen.send(path);
                }
            }
        }),
    );

    if let Err(err) = opened {
        say.note(format!("files: no file chooser on this machine ({err})"));
    }
}

/// Says that this machine has started controlling the far one, or stopped.
///
/// One place, because there are two ways to ask — the chord and the toolbar — and the flag the
/// loop holds and the picture in the title bar have to agree afterwards. Nothing about the
/// pointer changes here: it stays this machine's, visible and free, and what the flag decides
/// is only whether where it points is sent on.
fn announce_control(bar: Option<&toolbar::Toolbar>, say: &Reporter, on: bool) {
    if let Some(bar) = bar {
        bar.set_controlling(on);
    }

    say.note(if on {
        "display: controlling this machine, control option to stop"
    } else {
        "display: watching only, control option to take control"
    });
}

/// Opens a window and shows the stream until it ends or the window is closed.
///
/// Everything this would otherwise print goes to `say`, because the two callers want it in
/// different places: a terminal for one, a pipe to the shell for the other.
///
/// # Errors
///
/// Returns an error if SDL cannot start, the window or renderer cannot be created, or the
/// receive thread fails.
///
/// # Panics
///
/// Panics if the receive thread panicked.
pub fn run(
    mut config: ClientConfig,
    width: u32,
    height: u32,
    pacing_us: u32,
    capture_input: bool,
    synthetic_input: bool,
    say: &Reporter,
) -> Result<(), Box<dyn Error>> {
    let sdl = sdl3::init()?;
    let video = sdl.video()?;

    // Every pixel the screen has. Without this the window is drawn at one pixel to a point
    // and the compositor doubles it on a Retina display, which is a picture that arrives sharp
    // and is shown soft.
    let mut window = video
        .window("Prism", width, height)
        .position_centered()
        .resizable()
        .high_pixel_density()
        .build()?;

    // Installed before anything is drawn, because adding a toolbar moves the content view
    // down: a surface built for the window as it was would be built one title bar too tall.
    let mut bar = toolbar::Toolbar::install(&window);

    let (mut drawable_width, mut drawable_height) = window.size_in_pixels();
    let scale = f64::from(drawable_width) / f64::from(window.size().0.max(1));

    // The offer is what the host sizes its frames to, for the whole of the session: nothing
    // asks again when the window changes. So it is the most this window could come to show —
    // the largest screen here, in pixels — rather than what it happens to open at. Offering the
    // opening size was a stream sized for a window on whichever monitor it first appeared on,
    // stretched soft the moment somebody made it larger or filled the screen with it.
    let (most_across, most_down) =
        largest_screen(&video).unwrap_or((drawable_width, drawable_height));

    config.offer.max_width = u16::try_from(most_across.max(drawable_width)).unwrap_or(u16::MAX);
    config.offer.max_height = u16::try_from(most_down.max(drawable_height)).unwrap_or(u16::MAX);

    // Declared after the window so it is dropped before it: the surface holds objects the
    // window owns, and releasing them afterwards would be releasing them into nothing.
    let mut surface = surface::Surface::new(&window, drawable_width, drawable_height, scale)?;

    // After the surface, so the drawer is laid over the picture rather than under it.
    if let Some(bar) = bar.as_mut() {
        bar.add_drawer();
    }

    say.note(format!(
        "display: window {width}x{height}, drawable {drawable_width}x{drawable_height}, {}",
        surface.describe()
    ));

    // A machine with no sound device still shows picture. Audio is worth having and not worth
    // ending a session over, so a failure here is reported once and the stream carries on.
    let playback = match sdl
        .audio()
        .map_err(Box::<dyn Error>::from)
        .and_then(|audio| crate::audio::start(&audio))
    {
        Ok((playback, sink)) => Some((playback, sink)),
        Err(err) => {
            say.note(format!(
                "display: no audio output ({err}); the stream will be silent"
            ));
            None
        }
    };
    let audio_sink: Option<Arc<dyn client::Playback>> = playback
        .as_ref()
        .map(|(_, sink)| Arc::new(sink.clone()) as Arc<dyn client::Playback>);

    let (pictures_tx, pictures_rx) = sync_channel(PICTURE_QUEUE_DEPTH);
    let offset = Arc::new(AtomicI64::new(client::OFFSET_UNKNOWN));
    let input_slot: Arc<OnceLock<client::InputSender>> = Arc::new(OnceLock::new());
    let cursor_sink: client::CursorSink = Arc::new(Mutex::new(None));

    // Both directions of file movement, held here as well as by the session: this is what the
    // toolbar reaches for when somebody chooses a file, and the session is what carries it.
    let moving = transfer::shared_folder().map(|folder| Arc::new(Mutex::new(Files::new(folder))));
    let (landed_tx, landed_rx) = sync_channel(8);

    // What tells the session the window is gone. Without it the session only ends when the host
    // goes quiet, which a host that is still streaming never does.
    let leaving = Arc::new(AtomicBool::new(false));

    let worker = {
        let offset = Arc::clone(&offset);
        let input = Arc::clone(&input_slot);
        let cursor = Arc::clone(&cursor_sink);
        let files = moving.clone();
        let stop = Arc::clone(&leaving);
        #[cfg(target_os = "windows")]
        let gpu = surface.gpu();
        let report = say.clone();
        thread::spawn(move || {
            client::run(
                config,
                client::ClientHooks {
                    pictures: Some(pictures_tx),
                    offset: Some(offset),
                    input: Some(input),
                    cursor: Some(cursor),
                    audio: audio_sink,
                    report: Some(report),
                    files,
                    landed: Some(landed_tx),
                    stop: Some(stop),
                    #[cfg(target_os = "windows")]
                    gpu,
                },
            )
        })
    };

    let mut events = sdl.event_pump()?;
    let mut pacer = PresentPacer::new(pacing_us);
    let mut cursor = CursorTracker::new();
    let mut last_reading = None;
    let mut latency = LatencyRecorder::new(512);
    let mut shown = 0u64;
    let mut missed = 0u64;
    let mut last_hud = Instant::now();
    let mut hud_frames = 0u64;
    let mut sent_input = 0u64;

    // Whether what happens in this window is sent on to the far machine. On from the start,
    // because nothing is seized to make it so: the pointer stays this machine's, visible and
    // free to leave the window, and only where it points inside it goes across.
    let mut controlling = capture_input;

    // The size the pointer's coordinates are measured against, which is the window's own and
    // not the drawable's: the two differ on a screen with more than one pixel to a point.
    let mut area = window.size();

    // Whether the window is filling the screen, which SDL will not answer and this therefore
    // has to remember.
    let mut filling = false;

    // How large the pictures arriving are, once one has. What the window is set to when
    // somebody asks for the size the far machine is actually sending.
    let mut picture: Option<(u32, u32)> = None;

    // Where the file chooser puts what somebody picked. A channel rather than a shared slot,
    // because the dialog answers from inside the event pump and this loop reads it outside.
    let (chosen_tx, chosen_rx) = std::sync::mpsc::channel::<std::path::PathBuf>();

    if capture_input {
        announce_control(bar.as_ref(), say, controlling);
    }

    'main: loop {
        // Read from the window rather than kept here, because the control is not the only way
        // in or out of full screen, and a remembered answer goes stale the first time somebody
        // presses the green button instead.
        if let Some(bar) = bar.as_ref() {
            filling = bar.sync_fullscreen();
        }

        while let Some(tool) = bar.as_ref().and_then(toolbar::Toolbar::pressed) {
            match tool {
                toolbar::Tool::Control if capture_input => {
                    controlling = !controlling;
                    announce_control(bar.as_ref(), say, controlling);
                }
                // Asked for on a session that is only watching. Said rather than ignored,
                // because a control that does nothing when pressed is a fault to look for.
                toolbar::Tool::Control => {
                    say.note(
                        "display: this session is watching only, so there is nothing to control",
                    );
                }
                toolbar::Tool::Fit => {
                    if let Some((across, down)) = picture {
                        let _ = window.set_size(across, down);
                    }
                }
                toolbar::Tool::Fullscreen => {
                    filling = !filling;
                    let _ = window.set_fullscreen(filling);
                }
                toolbar::Tool::Send => choose_a_file(&window, &chosen_tx, say),
                toolbar::Tool::Fetch => {
                    if let Some(files) = moving.as_ref() {
                        if let Ok(mut files) = files.lock() {
                            files.ask_for_listing();
                        }
                    }
                }
                toolbar::Tool::Disconnect => break 'main,
            }
        }

        // What the file dialog came back with, if somebody has answered it since the last
        // turn. Offered here rather than in the callback, because the callback runs inside
        // SDL's event pump and the transfer is this thread's to start.
        while let Ok(path) = chosen_rx.try_recv() {
            let Some(files) = moving.as_ref() else {
                continue;
            };

            match files.lock().map(|mut files| files.send(&path)) {
                Ok(Ok(_)) => say.note(format!("files: sending {}", path.display())),
                Ok(Err(err)) => say.note(format!("files: {err}")),
                Err(_) => {}
            }
        }

        // A name picked out of the menu of what the far machine is offering.
        while let Some(name) = bar.as_ref().and_then(toolbar::Toolbar::chosen) {
            if let Some(files) = moving.as_ref() {
                if let Ok(mut files) = files.lock() {
                    files.fetch(&name);
                }
            }
        }

        while let Ok(event) = landed_rx.try_recv() {
            match event {
                Landed::Received { name, path } => {
                    say.note(format!("files: {name} arrived in {}", path.display()));
                }
                Landed::Listing(listing) => {
                    let files: Vec<(String, u64)> = listing
                        .files
                        .into_iter()
                        .map(|file| (file.name, file.size))
                        .collect();

                    if let Some(bar) = bar.as_ref() {
                        bar.offer(&files, listing.more);
                    }
                }
            }
        }

        for event in events.poll_iter() {
            if is_quit(&event) {
                break 'main;
            }
            if capture_input && is_control_toggle(&event) {
                controlling = !controlling;
                announce_control(bar.as_ref(), say, controlling);
                continue;
            }
            if resized(&event) {
                let (width, height) = window.size_in_pixels();
                if let Err(err) = surface.resize(width, height) {
                    say.note(format!("display: {err}"));
                    break 'main;
                }
                drawable_width = width;
                drawable_height = height;
                area = window.size();
            }
            if let (Event::MouseMotion { x, y, .. }, Some(bar)) = (&event, bar.as_ref()) {
                bar.pointer_moved(*x, *y);
            }

            // The drawer's controls are this window's, not the far machine's: a press or a pass
            // over them goes nowhere else. A release still does, so a drag that started on the
            // picture and ended over the drawer does not leave a button held over there.
            let over_drawer = pointer_at(&event)
                .is_some_and(|(x, y)| bar.as_ref().is_some_and(|bar| bar.covers(x, y)));

            if capture_input && ((controlling && !over_drawer) || is_release(&event)) {
                let shown = shown_in(picture, area);

                if let Some(sender) = input_slot.get() {
                    if controlling && !over_drawer {
                        if let Some(place) = where_clicked(&event, shown) {
                            if sender.send(place).is_ok() {
                                sent_input += 1;
                            }
                        }
                    }
                    if let Some(input) = to_input_event(&event, shown) {
                        if let Ok(stamped) = sender.send(input) {
                            sent_input += 1;
                            predict(&mut cursor, stamped, input);
                        }
                    }
                }
            }
        }

        if synthetic_input {
            if let Some(sender) = input_slot.get() {
                let motion = client::synthetic_motion(sent_input);
                if let Ok(stamped) = sender.send(motion) {
                    sent_input += 1;
                    predict(&mut cursor, stamped, motion);
                }
            }
        }

        // Corrections are applied where they are read rather than in the receive thread,
        // so the tracker stays owned by the one thread that draws it.
        if let Ok(slot) = cursor_sink.lock() {
            if *slot != last_reading {
                last_reading = *slot;
                if let Some(reading) = *slot {
                    cursor.observe(reading);
                }
            }
        }

        match pictures_rx.recv_timeout(Duration::from_millis(16)) {
            Ok(decoded) => {
                picture = Some(surface::size_of(&decoded));

                let clock_offset = offset.load(Ordering::Relaxed);
                if let Some(age) = client::age_of(surface::pts_of(&decoded), clock_offset) {
                    latency.record(age);
                    let hold = pacer.hold_for(age);
                    if !hold.is_zero() {
                        thread::sleep(hold);
                    }
                }

                if last_hud.elapsed() >= HUD_INTERVAL {
                    let rate = hud_frames as f64 / last_hud.elapsed().as_secs_f64();
                    let underway = moving
                        .as_ref()
                        .and_then(|files| files.lock().ok())
                        .map(|files| files.progress())
                        .unwrap_or_default();

                    surface.update_hud(&hud_lines(
                        &mut latency,
                        &mut pacer,
                        &Showing {
                            fps: rate,
                            shown,
                            missed,
                            clock_offset_us: clock_offset,
                            control: capture_input.then_some(controlling),
                        },
                        &underway,
                    ));
                    last_hud = Instant::now();
                    hud_frames = 0;
                }

                let target = (drawable_width as usize, drawable_height as usize);
                // The far pointer, where the host says it is, drawn whether or not this machine
                // is controlling it. It trails this machine's own pointer by a frame or two, and
                // that is the point of showing it: a far pointer that stays behind when this one
                // moves is the one sign on screen that the host is not doing what it is told.
                if surface.present(&decoded, cursor.normalised(), target)? {
                    shown += 1;
                    hud_frames += 1;
                } else {
                    missed += 1;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break 'main,
        }
    }

    // The window first, so leaving is instant whatever the session takes to wind down. A window
    // still on screen while this thread waits is a window the system draws the spinning pointer
    // over; one that has gone has nothing to draw it on.
    window.hide();
    leaving.store(true, Ordering::Relaxed);

    say.note(format!(
        "display: {shown} pictures shown, {missed} were dropped to stay in time"
    ));
    if capture_input {
        say.note(format!("input  : {sent_input} events sent"));
    }
    if let Some((_, sink)) = playback.as_ref() {
        let stats = sink.stats();
        say.note(format!(
            "audio  : {} frames played, {} concealed, {} starved, {} dropped, buffer {} frames",
            stats.played.load(Ordering::Relaxed),
            stats.concealed.load(Ordering::Relaxed),
            stats.starved.load(Ordering::Relaxed),
            stats.dropped.load(Ordering::Relaxed),
            stats.depth.load(Ordering::Relaxed),
        ));
    }
    report_pacing(say, &mut pacer);

    // Given a moment to finish, because its last lines are the session's own summary. Not
    // waited on past that: a host that has gone silent leaves the session in a read that only
    // its idle timeout ends, and the process is on its way out regardless.
    let deadline = Instant::now() + SESSION_WIND_DOWN;
    while !worker.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }

    if worker.is_finished() {
        worker
            .join()
            .expect("the receive thread should not panic")?;
    }

    Ok(())
}

/// How long a closed window waits for its session to report before leaving without it.
///
/// The session notices it has been told to stop within one packet, and a live host sends
/// several a second, so this is only ever reached when the host has already gone quiet.
const SESSION_WIND_DOWN: Duration = Duration::from_millis(1500);

#[cfg(test)]
mod tests {
    use super::{shown_in, to_fraction};

    /// A window the shape of the picture in it, so the picture fills it.
    fn filled() -> prism_core::render::Fitted {
        shown_in(Some((2560, 1504)), (1280, 752))
    }

    #[test]
    fn the_corners_of_the_window_are_the_corners_of_the_screen() {
        assert_eq!(to_fraction(0.0, 0.0, filled()), (0, 0));
        assert_eq!(to_fraction(1279.0, 751.0, filled()), (65535, 65535));
    }

    #[test]
    fn the_middle_of_the_window_is_the_middle_of_the_screen() {
        let (x, y) = to_fraction(639.5, 375.5, filled());

        assert!(x.abs_diff(u16::MAX / 2) <= 1, "{x}");
        assert!(y.abs_diff(u16::MAX / 2) <= 1, "{y}");
    }

    #[test]
    fn a_pointer_past_the_edge_stays_on_the_edge() {
        // Motion reported while the pointer is dragged out of the window, which SDL does for
        // as long as a button is held. Wrapping would put the far pointer on the opposite side.
        assert_eq!(to_fraction(-40.0, 900.0, filled()), (0, 65535));
        assert_eq!(to_fraction(5000.0, -1.0, filled()), (65535, 0));
    }

    #[test]
    fn the_corners_of_a_letterboxed_picture_are_the_corners_of_the_screen() {
        // A 16:9 screen in a square window: bars above and below, 218 points each.
        let shown = shown_in(Some((1920, 1080)), (1000, 1000));

        assert_eq!(to_fraction(0.0, 218.0, shown), (0, 0));
        assert_eq!(to_fraction(999.0, 780.0, shown), (65535, 65535));
        assert_eq!(
            to_fraction(500.0, 20.0, shown).1,
            0,
            "a click on the bar above is the top edge"
        );
    }

    #[test]
    fn before_the_first_picture_the_window_is_the_screen() {
        let shown = shown_in(None, (1280, 752));

        assert_eq!(to_fraction(1279.0, 751.0, shown), (65535, 65535));
    }
}
