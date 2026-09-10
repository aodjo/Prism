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
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use prism_core::cursor::CursorTracker;
use prism_core::net::packet::{InputEvent, MouseButton};
use prism_core::render::pacing::PresentPacer;
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
const HUD_HEIGHT: usize = 140;

/// How large the statistics panel's text is, in pixels.
const HUD_FONT_SIZE: f64 = 13.0;

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

/// Returns whether an event is the chord that takes the pointer, or hands it back.
///
/// Control and option together, pressed with nothing else, which is what every other machine
/// on a desk uses for the same thing. Shift is excluded so that reaching for the quit chord
/// does not release the pointer on the way.
///
/// Read from the modifiers rather than from the key, so it does not matter which of the two
/// went down first or whether they are the left or the right one.
fn is_grab_toggle(event: &Event) -> bool {
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

/// Returns whether an event is a key being let go.
///
/// Sent on whether or not the pointer is the host's, because the chord that hands it back is
/// two keys held down: the host saw them pressed and would go on holding them if the release
/// stayed on this machine.
fn is_release(event: &Event) -> bool {
    matches!(event, Event::KeyUp { .. })
}

/// Returns whether an event is a click inside the picture.
///
/// What takes the pointer without a chord, the way a machine on the desk is used: somebody
/// who clicks the screen they are watching means to be working on it.
fn is_click(event: &Event) -> bool {
    matches!(event, Event::MouseButtonDown { .. })
}

/// Translates one SDL event into an input event for the host, if it is one.
///
/// Key repeats are dropped. The host's own operating system generates repeats from the
/// key being held, so forwarding the client's as well would double them.
fn to_input_event(event: &Event) -> Option<InputEvent> {
    match event {
        Event::MouseMotion { xrel, yrel, .. } => Some(InputEvent::MouseMove {
            dx: *xrel as i16,
            dy: *yrel as i16,
        }),
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

/// Builds the lines the overlay shows.
///
/// Latency first, because it is what every milestone is judged on, and the pacing line
/// directly beneath it because that delay is part of the number above and should not have
/// to be remembered separately.
fn hud_lines(
    latency: &mut LatencyRecorder,
    pacer: &mut PresentPacer,
    fps: f64,
    shown: u64,
    missed: u64,
    clock_offset_us: i64,
    control: Option<bool>,
) -> Vec<String> {
    let mut lines = Vec::with_capacity(6);

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
        Some(true) => lines.push("control  on, control option to let go".to_owned()),
        Some(false) => lines.push("control  off, click to take it".to_owned()),
        None => {}
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
    config: ClientConfig,
    width: u32,
    height: u32,
    pacing_us: u32,
    capture_input: bool,
    synthetic_input: bool,
    say: &Reporter,
) -> Result<(), Box<dyn Error>> {
    let sdl = sdl3::init()?;
    let video = sdl.video()?;
    let window = video
        .window("Prism", width, height)
        .position_centered()
        .resizable()
        .build()?;

    let (mut drawable_width, mut drawable_height) = window.size_in_pixels();

    // Declared after the window so it is dropped before it: the surface holds objects the
    // window owns, and releasing them afterwards would be releasing them into nothing.
    let mut surface = surface::Surface::new(&window, drawable_width, drawable_height)?;

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
    let worker = {
        let offset = Arc::clone(&offset);
        let input = Arc::clone(&input_slot);
        let cursor = Arc::clone(&cursor_sink);
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

    // Whether the pointer and keyboard are the host's right now. Off to begin with: a window
    // that seized the mouse the moment it opened would be one somebody had to know a chord to
    // escape from before they had seen the screen they came for.
    let mut grabbed = false;

    if capture_input {
        say.note("display: click to control this machine, control option to let go");
    }

    'main: loop {
        for event in events.poll_iter() {
            if is_quit(&event) {
                break 'main;
            }
            if capture_input && (is_grab_toggle(&event) || (!grabbed && is_click(&event))) {
                grabbed = !grabbed;
                sdl.mouse().set_relative_mouse_mode(&window, grabbed);
                say.note(if grabbed {
                    "display: controlling this machine, control option to let go"
                } else {
                    "display: watching only, click to control this machine"
                });
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
            }
            if capture_input && (grabbed || is_release(&event)) {
                if let Some(sender) = input_slot.get() {
                    if let Some(input) = to_input_event(&event) {
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
            Ok(picture) => {
                let clock_offset = offset.load(Ordering::Relaxed);
                if let Some(age) = client::age_of(surface::pts_of(&picture), clock_offset) {
                    latency.record(age);
                    let hold = pacer.hold_for(age);
                    if !hold.is_zero() {
                        thread::sleep(hold);
                    }
                }

                if last_hud.elapsed() >= HUD_INTERVAL {
                    let rate = hud_frames as f64 / last_hud.elapsed().as_secs_f64();
                    surface.update_hud(&hud_lines(
                        &mut latency,
                        &mut pacer,
                        rate,
                        shown,
                        missed,
                        clock_offset,
                        capture_input.then_some(grabbed),
                    ));
                    last_hud = Instant::now();
                    hud_frames = 0;
                }

                let target = (drawable_width as usize, drawable_height as usize);
                if surface.present(&picture, cursor.normalised(), target)? {
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
    worker
        .join()
        .expect("the receive thread should not panic")?;

    Ok(())
}
