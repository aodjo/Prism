//! The screen's frames, from a PipeWire stream.
//!
//! The portal hands over a PipeWire remote and the node that is the screen; this connects to
//! that node and keeps the newest frame it delivers. PipeWire runs its own loop, and that loop
//! wants a thread of its own that does nothing else — its callbacks are where the compositor's
//! buffers are handed over and must be handed back promptly, or the compositor stalls on a
//! consumer that is slow to let go.
//!
//! So the callback does one thing: copies the frame into the newest-frame slot and returns the
//! buffer. The pump takes that slot by swapping it for one it has finished with, which is a
//! pointer exchange under a lock rather than a copy, and converts it at its own pace. A frame the
//! pump did not get to before the next arrived is simply replaced — the same newest-wins rule as
//! every other queue on the path.
//!
//! # Buffers in memory, not on the GPU
//!
//! The formats offered here say nothing about DMA-BUF modifiers, which is how a PipeWire consumer
//! asks for frames in memory the processor can read. The encoder that runs on every Linux machine
//! reads from memory, so that is where they are wanted; a frame imported from the GPU would only
//! have to be read back.

#![cfg(linux_desktop)]

use std::os::fd::OwnedFd;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use pipewire as pw;
use pw::spa;
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::Pod;

use crate::capture::CaptureError;
use crate::yuv::PixelOrder;

/// How long to wait for PipeWire to agree a format before giving up on the stream.
const FORMAT_PATIENCE: Duration = Duration::from_secs(5);

/// The largest screen offered for, in each direction.
///
/// OpenH264 stops at 4096, so there is nothing to gain from a larger frame, and naming a limit is
/// what lets the compositor choose a size at all.
const LARGEST: u32 = 4096;

/// A frame, as it arrived.
#[derive(Default)]
pub struct Frame {
    /// The pixels, `stride` bytes a row.
    pub bytes: Vec<u8>,
    /// Bytes from the start of one row to the start of the next.
    pub stride: usize,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Where the colours are in each pixel.
    pub order: Option<PixelOrder>,
}

impl core::fmt::Debug for Frame {
    /// Names the frame's shape, not its pixels.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Frame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride", &self.stride)
            .finish_non_exhaustive()
    }
}

/// What the PipeWire thread and the pump share.
#[derive(Default)]
struct Shared {
    slot: Mutex<Slot>,
    arrived: Condvar,
}

/// The newest frame, and what is known about the stream.
#[derive(Default)]
struct Slot {
    frame: Frame,
    /// Whether `frame` is newer than the last one taken.
    fresh: bool,
    /// The size and order PipeWire agreed, once it has.
    format: Option<(u32, u32, PixelOrder)>,
    /// Why the stream stopped, once it has.
    failed: Option<String>,
}

/// A running stream of the screen.
///
/// Dropping it stops the PipeWire loop and waits for its thread.
pub struct Frames {
    shared: Arc<Shared>,
    quit: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Frames {
    /// Connects to a node and waits for PipeWire to agree a format.
    ///
    /// `remote` is the connection the portal opened; `None` connects to this user's own PipeWire
    /// instead, which is what a test does with a node it made itself.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::Start`] if the stream cannot be made or connected, or no format
    /// is agreed within a few seconds.
    pub fn start(remote: Option<OwnedFd>, node: u32, fps: u32) -> Result<Self, CaptureError> {
        let shared = Arc::new(Shared::default());
        let (quit, told) = pw::channel::channel::<()>();
        let (started, running) = mpsc::channel::<Result<(), String>>();

        let thread = {
            let shared = Arc::clone(&shared);

            std::thread::Builder::new()
                .name("prism-pipewire".into())
                .spawn(move || {
                    if let Err(reason) = run(remote, node, fps.max(1), &shared, told, &started) {
                        let _ = started.send(Err(reason.clone()));

                        if let Ok(mut slot) = shared.slot.lock() {
                            slot.failed = Some(reason);
                        }

                        shared.arrived.notify_all();
                    }
                })
                .map_err(|err| failed(err.to_string()))?
        };

        let frames = Self {
            shared,
            quit,
            thread: Some(thread),
        };

        running
            .recv_timeout(FORMAT_PATIENCE)
            .map_err(|_| failed("PipeWire never started the stream".to_owned()))?
            .map_err(failed)?;

        frames.agreed(FORMAT_PATIENCE)?;

        Ok(frames)
    }

    /// Waits for the size PipeWire agreed.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::Start`] if the stream failed or nothing was agreed in time.
    pub fn agreed(&self, patience: Duration) -> Result<(u32, u32), CaptureError> {
        let slot = self.shared.slot.lock().map_err(|_| poisoned())?;
        let (slot, _) = self
            .shared
            .arrived
            .wait_timeout_while(slot, patience, |slot| {
                slot.format.is_none() && slot.failed.is_none()
            })
            .map_err(|_| poisoned())?;

        if let Some(reason) = &slot.failed {
            return Err(failed(reason.clone()));
        }

        slot.format
            .map(|(width, height, _)| (width, height))
            .ok_or_else(|| failed("PipeWire agreed no format for the screen".to_owned()))
    }

    /// Takes the newest frame, if one has arrived since the last, into `into`.
    ///
    /// Swaps rather than copies: what `into` held goes back to be written into next. Returns
    /// whether there was a new frame, waiting up to `patience` for one.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::Start`] with PipeWire's reason once the stream has stopped.
    pub fn take(&self, into: &mut Frame, patience: Duration) -> Result<bool, CaptureError> {
        let slot = self.shared.slot.lock().map_err(|_| poisoned())?;
        let (mut slot, _) = self
            .shared
            .arrived
            .wait_timeout_while(slot, patience, |slot| !slot.fresh && slot.failed.is_none())
            .map_err(|_| poisoned())?;

        if let Some(reason) = &slot.failed {
            return Err(failed(reason.clone()));
        }

        if !slot.fresh {
            return Ok(false);
        }

        slot.fresh = false;
        core::mem::swap(&mut slot.frame, into);

        Ok(true)
    }
}

impl Drop for Frames {
    /// Stops the loop and waits for its thread, so the stream is gone when this returns.
    fn drop(&mut self) {
        let _ = self.quit.send(());

        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl core::fmt::Debug for Frames {
    /// Names the type, not the PipeWire objects behind it.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Frames").finish_non_exhaustive()
    }
}

/// The PipeWire thread: connect, say so, and run until told to stop.
fn run(
    remote: Option<OwnedFd>,
    node: u32,
    fps: u32,
    shared: &Arc<Shared>,
    told: pw::channel::Receiver<()>,
    started: &mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|err| err.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|err| err.to_string())?;
    let core = match remote {
        Some(fd) => context.connect_fd_rc(fd, None),
        None => context.connect_rc(None),
    }
    .map_err(|err| format!("could not reach PipeWire: {err}"))?;

    let stream = pw::stream::StreamBox::new(
        &core,
        "prism-screen",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|err| err.to_string())?;

    let on_format = Arc::clone(shared);
    let on_frame = Arc::clone(shared);
    let on_state = Arc::clone(shared);

    let _listener = stream
        .add_local_listener_with_user_data(VideoInfoRaw::default())
        .state_changed(move |_, _, _, now| {
            if let pw::stream::StreamState::Error(reason) = now {
                if let Ok(mut slot) = on_state.slot.lock() {
                    slot.failed = Some(format!("the screen's stream stopped: {reason}"));
                }

                on_state.arrived.notify_all();
            }
        })
        .param_changed(move |_, info, id, param| {
            let Some(param) = param else {
                return;
            };

            if id != spa::param::ParamType::Format.as_raw() || info.parse(param).is_err() {
                return;
            }

            let Some(order) = order_of(info.format()) else {
                return;
            };

            if let Ok(mut slot) = on_format.slot.lock() {
                slot.format = Some((info.size().width, info.size().height, order));
            }

            on_format.arrived.notify_all();
        })
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };

            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };

            let chunk = data.chunk();
            let (offset, size, stride) = (
                chunk.offset() as usize,
                chunk.size() as usize,
                usize::try_from(chunk.stride()).unwrap_or(0),
            );

            let Some(bytes) = data.data() else {
                return;
            };

            let Some(pixels) = bytes.get(offset..offset + size) else {
                return;
            };

            let Ok(mut slot) = on_frame.slot.lock() else {
                return;
            };

            let Some((width, height, order)) = slot.format else {
                return;
            };

            // Resized only when the screen's size or stride changes, which is not per frame.
            let frame = &mut slot.frame;
            frame.bytes.resize(pixels.len(), 0);
            frame.bytes.copy_from_slice(pixels);
            frame.stride = if stride == 0 {
                width as usize * 4
            } else {
                stride
            };
            frame.width = width;
            frame.height = height;
            frame.order = Some(order);
            slot.fresh = true;

            drop(slot);
            on_frame.arrived.notify_all();
        })
        .register()
        .map_err(|err| err.to_string())?;

    let offer = formats(fps)?;
    let mut params = [Pod::from_bytes(&offer).ok_or("the format offer did not serialise")?];

    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|err| format!("could not connect to the screen's stream: {err}"))?;

    let _stop = told.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();

        move |()| mainloop.quit()
    });

    let _ = started.send(Ok(()));

    mainloop.run();

    Ok(())
}

/// The formats this end can read, as the pod PipeWire negotiates against.
///
/// Every four-byte order the conversion knows, any size up to [`LARGEST`], and any rate up to the
/// session's own — asking for more frames than will be sent is work the compositor does for
/// nobody.
fn formats(fps: u32) -> Result<Vec<u8>, String> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::pod::{Value, object, property, serialize::PodSerializer};
    use spa::utils::{Fraction, Rectangle, SpaTypes};

    let offer = object!(
        SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Video),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
            VideoFormat::xRGB,
            VideoFormat::ARGB,
            VideoFormat::xBGR,
            VideoFormat::ABGR,
        ),
        property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 1920,
                height: 1080
            },
            Rectangle {
                width: 2,
                height: 2
            },
            Rectangle {
                width: LARGEST,
                height: LARGEST
            }
        ),
        property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: fps, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: fps, denom: 1 }
        ),
    );

    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(offer))
        .map(|(written, _)| written.into_inner())
        .map_err(|err| format!("the format offer did not serialise: {err:?}"))
}

/// Where the colours sit, for a format this end offered.
fn order_of(format: VideoFormat) -> Option<PixelOrder> {
    match format {
        VideoFormat::BGRx | VideoFormat::BGRA => Some(PixelOrder::BGRX),
        VideoFormat::RGBx | VideoFormat::RGBA => Some(PixelOrder::RGBX),
        VideoFormat::xRGB | VideoFormat::ARGB => Some(PixelOrder::XRGB),
        VideoFormat::xBGR | VideoFormat::ABGR => Some(PixelOrder::XBGR),
        _ => None,
    }
}

/// A stream that could not be started, with the reason.
fn failed(reason: String) -> CaptureError {
    CaptureError::Start { reason }
}

/// A lock whose holder panicked, which only a bug here can cause.
fn poisoned() -> CaptureError {
    failed("the screen's frame slot was poisoned".to_owned())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Frame, Frames};

    /// Connects to a video source this user's PipeWire already has, and reads frames from it.
    ///
    /// The source stands in for the portal's screen: any PipeWire node producing raw video will
    /// do, and `gst-launch-1.0 videotestsrc ! video/x-raw,format=BGRx,width=640,height=360 !
    /// pipewiresink` makes one. Its node id goes in `PRISM_TEST_PIPEWIRE_NODE`.
    #[test]
    #[ignore = "needs a PipeWire daemon with a video source; see the comment"]
    fn frames_arrive_from_a_video_source() {
        let node = std::env::var("PRISM_TEST_PIPEWIRE_NODE")
            .expect("PRISM_TEST_PIPEWIRE_NODE")
            .parse()
            .expect("a node id");

        let frames = Frames::start(None, node, 30).expect("the stream starts");
        let (width, height) = frames.agreed(Duration::from_secs(1)).expect("a format");

        let mut frame = Frame::default();
        let mut taken = 0;

        for _ in 0..20 {
            if frames
                .take(&mut frame, Duration::from_millis(500))
                .expect("no failure")
            {
                taken += 1;
            }
        }

        assert!(taken >= 5, "only {taken} frames in ten seconds");
        assert_eq!((frame.width, frame.height), (width, height));
        assert!(frame.stride >= width as usize * 4);
        assert!(frame.bytes.len() >= frame.stride * (height as usize - 1));
        assert!(frame.order.is_some());
    }
}
