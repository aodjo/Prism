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
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::thread;
use std::time::Duration;

use objc2_metal::MTLPixelFormat;
use objc2_quartz_core::CAMetalLayer;
use prism_core::render::metal::MetalRenderer;
use prism_core::render::pacing::PresentPacer;
use sdl3::event::Event;
use sdl3::keyboard::Keycode;
use sdl3_sys::metal::{SDL_Metal_CreateView, SDL_Metal_DestroyView, SDL_Metal_GetLayer};

use crate::client::{self, ClientConfig};

/// How many pictures may wait to be shown before the newest is dropped.
const PICTURE_QUEUE_DEPTH: usize = 2;

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

    let (pictures_tx, pictures_rx) = sync_channel(PICTURE_QUEUE_DEPTH);
    let offset = Arc::new(AtomicI64::new(client::OFFSET_UNKNOWN));
    let worker = {
        let offset = Arc::clone(&offset);
        thread::spawn(move || client::run(config, Some(pictures_tx), offset))
    };

    let mut events = sdl.event_pump()?;
    let mut pacer = PresentPacer::new(pacing_us);
    let mut shown = 0u64;
    let mut missed = 0u64;

    'main: loop {
        for event in events.poll_iter() {
            match event {
                Event::Quit { .. }
                | Event::KeyDown {
                    keycode: Some(Keycode::Escape),
                    ..
                } => break 'main,
                _ => {}
            }
        }

        match pictures_rx.recv_timeout(Duration::from_millis(16)) {
            Ok(picture) => {
                if let Some(age) = client::age_of(picture.pts_us, offset.load(Ordering::Relaxed)) {
                    let hold = pacer.hold_for(age);
                    if !hold.is_zero() {
                        thread::sleep(hold);
                    }
                }

                if renderer.present(picture.pixel_buffer(), layer)? {
                    shown += 1;
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
    report_pacing(&mut pacer);
    worker
        .join()
        .expect("the receive thread should not panic")?;

    Ok(())
}
