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

use std::error::Error;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use objc2_metal::MTLPixelFormat;
use objc2_quartz_core::CAMetalLayer;
use prism_core::cursor::CursorTracker;
use prism_core::net::packet::{InputEvent, MouseButton};
use prism_core::render::cursor::CursorOverlay;
use prism_core::render::metal::MetalRenderer;
use prism_core::render::overlay::TextOverlay;
use prism_core::render::pacing::PresentPacer;
use prism_core::stats::LatencyRecorder;
use sdl3::event::Event;
use sdl3::keyboard::{Keycode, Mod};
use sdl3::mouse::MouseButton as SdlMouseButton;
use sdl3_sys::metal::{SDL_Metal_CreateView, SDL_Metal_DestroyView, SDL_Metal_GetLayer};

use crate::client::{self, ClientConfig};

/// How many pictures may wait to be shown before the newest is dropped.
const PICTURE_QUEUE_DEPTH: usize = 2;

/// How often the statistics overlay is redrawn.
///
/// Ten times a second. Faster would be unreadable and would put CPU text rasterisation on
/// a path that exists to avoid exactly that kind of work.
const HUD_INTERVAL: Duration = Duration::from_millis(100);

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
) -> Vec<String> {
    let mut lines = Vec::with_capacity(5);

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

    lines
}

/// Prints what the pacer cost and what it bought.
///
/// The delay it added belongs next to the end-to-end latency rather than hidden inside
/// it: smoothness is bought with latency, and the price should be visible.
fn report_pacing(pacer: &mut PresentPacer) {
    if !pacer.enabled() {
        println!(
            "pacing : off — every picture was shown the moment it decoded ({} pictures)",
            pacer.total()
        );
        return;
    }

    let target = pacer.delay_us();
    let late = pacer.shown_late();
    let total = pacer.total();

    let Some(held) = pacer.held_summary() else {
        println!("pacing : no pictures were paced");
        return;
    };

    println!(
        "pacing : target {:.2} ms, held p50 {:.2} p99 {:.2} max {:.2} ms, {late}/{total} arrived late",
        f64::from(target) / 1000.0,
        f64::from(held.p50_us) / 1000.0,
        f64::from(held.p99_us) / 1000.0,
        f64::from(held.max_us) / 1000.0,
    );
}

/// Opens a window and shows the stream until it ends or the window is closed.
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
) -> Result<(), Box<dyn Error>> {
    let sdl = sdl3::init()?;
    let video = sdl.video()?;
    let window = video
        .window("Prism", width, height)
        .position_centered()
        .build()?;

    // SAFETY: the window outlives the view, which is destroyed before this returns.
    let view = unsafe { SDL_Metal_CreateView(window.raw()) };
    if view.is_null() {
        return Err("could not create a Metal view for the window".into());
    }

    // SAFETY: SDL returns the view's CAMetalLayer, which lives as long as the view.
    let layer = unsafe { &*(SDL_Metal_GetLayer(view).cast::<CAMetalLayer>()) };

    let mut renderer = MetalRenderer::new(MTLPixelFormat::BGRA8Unorm)?;
    let (drawable_width, drawable_height) = window.size_in_pixels();
    renderer.configure_layer(layer, drawable_width as usize, drawable_height as usize);

    println!(
        "display: window {width}x{height}, drawable {drawable_width}x{drawable_height}, escape to quit"
    );

    // A machine with no sound device still shows picture. Audio is worth having and not worth
    // ending a session over, so a failure here is reported once and the stream carries on.
    let playback = match sdl
        .audio()
        .map_err(Box::<dyn Error>::from)
        .and_then(|audio| crate::audio::start(&audio))
    {
        Ok((playback, sink)) => Some((playback, sink)),
        Err(err) => {
            eprintln!("display: no audio output ({err}); the stream will be silent");
            None
        }
    };
    let audio_sink = playback.as_ref().map(|(_, sink)| sink.clone());

    let (pictures_tx, pictures_rx) = sync_channel(PICTURE_QUEUE_DEPTH);
    let offset = Arc::new(AtomicI64::new(client::OFFSET_UNKNOWN));
    let input_slot: Arc<OnceLock<client::InputSender>> = Arc::new(OnceLock::new());
    let cursor_sink: client::CursorSink = Arc::new(Mutex::new(None));
    let worker = {
        let offset = Arc::clone(&offset);
        let input = Arc::clone(&input_slot);
        let cursor = Arc::clone(&cursor_sink);
        thread::spawn(move || {
            client::run(
                config,
                client::ClientHooks {
                    pictures: Some(pictures_tx),
                    offset: Some(offset),
                    input: Some(input),
                    cursor: Some(cursor),
                    audio: audio_sink,
                },
            )
        })
    };

    let mut events = sdl.event_pump()?;
    let mut pacer = PresentPacer::new(pacing_us);
    let mut overlay = TextOverlay::new(renderer.device(), 340, 118, 13.0)?;
    let cursor_bitmap = CursorOverlay::new(renderer.device())?;
    let mut cursor = CursorTracker::new();
    let mut last_reading = None;
    let mut latency = LatencyRecorder::new(512);
    let mut shown = 0u64;
    let mut missed = 0u64;
    let mut last_hud = Instant::now();
    let mut hud_frames = 0u64;
    let mut sent_input = 0u64;

    if capture_input {
        sdl.mouse().set_relative_mouse_mode(&window, true);
        println!("display: forwarding input, control alt shift Q to quit");
    }

    'main: loop {
        for event in events.poll_iter() {
            if is_quit(&event) {
                break 'main;
            }
            if capture_input {
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
                if let Some(age) = client::age_of(picture.pts_us, clock_offset) {
                    latency.record(age);
                    let hold = pacer.hold_for(age);
                    if !hold.is_zero() {
                        thread::sleep(hold);
                    }
                }

                if last_hud.elapsed() >= HUD_INTERVAL {
                    let rate = hud_frames as f64 / last_hud.elapsed().as_secs_f64();
                    overlay.update(&hud_lines(
                        &mut latency,
                        &mut pacer,
                        rate,
                        shown,
                        missed,
                        clock_offset,
                    ));
                    last_hud = Instant::now();
                    hud_frames = 0;
                }

                // A fixed array rather than a vector: this runs once per displayed frame,
                // and the frame path does not allocate.
                //
                // The cursor comes after the statistics so it draws on top of them. It is
                // the thing being pointed with, and it should never vanish behind a panel.
                let target = (drawable_width as usize, drawable_height as usize);
                let mut quads = [overlay.quad(target.0, target.1); 2];
                let count = match cursor.normalised() {
                    Some(at) => {
                        quads[1] = cursor_bitmap.quad(at, target.0, target.1);
                        2
                    }
                    None => 1,
                };

                if renderer.present(picture.pixel_buffer(), layer, &quads[..count])? {
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

    // SAFETY: the layer is not used after this point, and the window still outlives it.
    unsafe { SDL_Metal_DestroyView(view) };

    println!("display: {shown} pictures shown, {missed} had no drawable available");
    if capture_input {
        println!("input  : {sent_input} events sent");
    }
    if let Some((_, sink)) = playback.as_ref() {
        let stats = sink.stats();
        println!(
            "audio  : {} frames played, {} concealed, {} starved, {} dropped, buffer {} frames",
            stats.played.load(Ordering::Relaxed),
            stats.concealed.load(Ordering::Relaxed),
            stats.starved.load(Ordering::Relaxed),
            stats.dropped.load(Ordering::Relaxed),
            stats.depth.load(Ordering::Relaxed),
        );
    }
    report_pacing(&mut pacer);
    worker
        .join()
        .expect("the receive thread should not panic")?;

    Ok(())
}
