//! Capturing whatever a Windows machine is playing.
//!
//! WASAPI has a loopback mode: instead of reading a microphone, a capture client attached to a
//! *render* device receives the mix that device is about to play. That is exactly what a
//! remote desktop wants — everything the person at the other end would have heard, from every
//! application, without asking any of them to cooperate.
//!
//! # Silence has to be invented
//!
//! A loopback client receives nothing at all while the machine is silent. Not zeroes: nothing.
//! An encoder fed only what arrives would therefore stop, and the stream would stall until
//! something made a sound — after which the client's jitter buffer would be filling from empty
//! at exactly the moment somebody wanted to hear something. So a gap is filled with silence
//! and the stream runs continuously, which is also what keeps the two clocks comparable.
//!
//! # Rate conversion
//!
//! The device runs at whatever rate it was configured with, usually 48 kHz and sometimes
//! 44.1. Opus works in 48. Rather than resample — which costs quality and a filter — the
//! capture client is asked for 48 kHz float and WASAPI's own mixer converts, which it is doing
//! anyway for every application on the machine.

#![cfg(target_os = "windows")]

use std::time::Duration;

use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient, IAudioClient,
    IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVEFORMATEXTENSIBLE_0, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::audio::{CHANNELS, FRAME_INTERLEAVED, SAMPLE_RATE};

/// How long the device buffer holds, in hundred-nanosecond units.
///
/// Twenty milliseconds. Short enough that a stall in this thread shows up as a dropout rather
/// than as latency nobody notices until it is a hundred milliseconds deep, long enough that
/// ordinary scheduling jitter does not overrun it.
const BUFFER_DURATION: i64 = 20 * 10_000;

/// Reason audio capture could not start or continue.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AudioCaptureError {
    /// Windows refused a call.
    #[error("{what} failed: {code}")]
    Windows {
        /// Which call.
        what: &'static str,
        /// What it said.
        code: String,
    },
}

/// Turns a Windows failure into one of ours.
fn failed(what: &'static str, error: windows::core::Error) -> AudioCaptureError {
    AudioCaptureError::Windows {
        what,
        code: error.to_string(),
    }
}

/// Captures the machine's audio output.
///
/// Not `Send`: the COM apartment it initialises belongs to the thread that made it, and moving
/// it elsewhere would use an apartment that thread does not have.
pub struct LoopbackCapture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    ready: HANDLE,
    /// Samples that arrived but did not fill a frame, kept for the next one.
    ///
    /// WASAPI delivers whatever the device produced, which is not a multiple of the frame size.
    /// Without this the remainder would be dropped and the stream would slowly drift.
    pending: Vec<f32>,
    /// The frame handed to the caller, reused rather than allocated per frame.
    frame: Box<[f32; FRAME_INTERLEAVED]>,
}

impl LoopbackCapture {
    /// Starts capturing the default output device.
    ///
    /// # Errors
    ///
    /// Returns [`AudioCaptureError::Windows`] if there is no output device, or if it refuses
    /// the format — which for a shared-mode client asking for what the mixer already produces
    /// should not happen, and if it does the session is better off knowing than guessing.
    pub fn start() -> Result<Self, AudioCaptureError> {
        // SAFETY: initialising the apartment for this thread, which every later call here
        // requires. Multithreaded because nothing in this crate pumps a message loop.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|error| failed("CoInitializeEx", error))?;

        // SAFETY: the enumerator is a standard COM object and the interface identifier is the
        // one its type names.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|error| failed("CoCreateInstance", error))?;

        // SAFETY: asking for the device the machine plays through, which is what loopback
        // capture attaches to.
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .map_err(|error| failed("GetDefaultAudioEndpoint", error))?;

        // SAFETY: the device is live and the interface identifier matches the binding.
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(|error| failed("Activate", error))?;

        let format = wanted_format();

        // SAFETY: the format is fully initialised above and outlives the call. The automatic
        // conversion flags are what let a device running at another rate still deliver the
        // 48 kHz float this asks for, using the mixer that is already resampling for every
        // application on the machine.
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_DURATION,
                0,
                std::ptr::addr_of!(format.Format),
                None,
            )
        }
        .map_err(|error| failed("IAudioClient::Initialize", error))?;

        // SAFETY: an unnamed auto-reset event, which the client signals when a buffer is ready.
        let ready = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|error| failed("CreateEventW", error))?;

        // SAFETY: the event outlives the client, which is dropped in this type's destructor.
        unsafe { client.SetEventHandle(ready) }.map_err(|error| failed("SetEventHandle", error))?;

        // SAFETY: the client is initialised and the interface identifier matches the binding.
        let capture: IAudioCaptureClient =
            unsafe { client.GetService() }.map_err(|error| failed("GetService", error))?;

        // SAFETY: everything the client needs was set above.
        unsafe { client.Start() }.map_err(|error| failed("IAudioClient::Start", error))?;

        Ok(Self {
            client,
            capture,
            ready,
            pending: Vec::with_capacity(FRAME_INTERLEAVED * 4),
            frame: Box::new([0.0; FRAME_INTERLEAVED]),
        })
    }

    /// Returns the next frame, waiting up to `timeout` for the device.
    ///
    /// A timeout is not a failure: it means the machine is silent, and the caller fills the gap
    /// with silence of its own so the stream keeps running. See the module's note on why that
    /// matters.
    ///
    /// # Errors
    ///
    /// Returns [`AudioCaptureError::Windows`] if the device stops.
    pub fn poll(&mut self, timeout: Duration) -> Result<Option<&[f32]>, AudioCaptureError> {
        // Anything left from last time may already be a whole frame.
        if self.pending.len() >= FRAME_INTERLEAVED {
            return Ok(Some(self.take_frame()));
        }

        // SAFETY: the handle was created above and is closed only in the destructor.
        let waited = unsafe {
            WaitForSingleObject(
                self.ready,
                timeout.as_millis().min(u128::from(u32::MAX)) as u32,
            )
        };

        if waited != WAIT_OBJECT_0 {
            return Ok(None);
        }

        self.drain()?;

        Ok((self.pending.len() >= FRAME_INTERLEAVED).then(|| self.take_frame()))
    }

    /// Moves everything the device has ready into the pending buffer.
    fn drain(&mut self) -> Result<(), AudioCaptureError> {
        loop {
            // SAFETY: the client is live for the life of this type.
            let available = unsafe { self.capture.GetNextPacketSize() }
                .map_err(|error| failed("GetNextPacketSize", error))?;

            if available == 0 {
                return Ok(());
            }

            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;

            // SAFETY: the three outputs are live locals, and the buffer the call hands back is
            // read only up to the frame count it reports and released immediately after.
            unsafe {
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
            }
            .map_err(|error| failed("GetBuffer", error))?;

            let samples = frames as usize * CHANNELS;

            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                // The device says this run is silent and the buffer's contents are undefined,
                // so the zeroes are written rather than read.
                self.pending.resize(self.pending.len() + samples, 0.0);
            } else {
                // SAFETY: the pointer is the buffer just handed back, and it holds exactly the
                // reported number of frames in the format the client was initialised with.
                let borrowed = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), samples) };
                self.pending.extend_from_slice(borrowed);
            }

            // SAFETY: releasing exactly what was taken, which the interface requires before the
            // next call.
            unsafe { self.capture.ReleaseBuffer(frames) }
                .map_err(|error| failed("ReleaseBuffer", error))?;
        }
    }

    /// Takes one frame off the front of the pending buffer.
    fn take_frame(&mut self) -> &[f32] {
        self.frame
            .copy_from_slice(&self.pending[..FRAME_INTERLEAVED]);
        self.pending.drain(..FRAME_INTERLEAVED);

        self.frame.as_slice()
    }
}

impl Drop for LoopbackCapture {
    /// Stops the client and leaves the apartment.
    fn drop(&mut self) {
        // SAFETY: the client was started in the constructor and this runs once.
        unsafe {
            let _ = self.client.Stop();
            CoUninitialize();
        }
    }
}

impl core::fmt::Debug for LoopbackCapture {
    /// Describes the capture without printing a COM pointer.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoopbackCapture")
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

/// Builds the format the capture client is asked for.
///
/// Extensible rather than plain, because a plain `WAVEFORMATEX` cannot name a channel mask and
/// a shared-mode client without one is at the mixer's discretion about which speaker is which.
fn wanted_format() -> WAVEFORMATEXTENSIBLE {
    const BITS: u16 = 32;

    let block_align = (CHANNELS as u16) * BITS / 8;

    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: CHANNELS as u16,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * u32::from(block_align),
            nBlockAlign: block_align,
            wBitsPerSample: BITS,
            cbSize: (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: BITS,
        },
        // Front left and front right.
        dwChannelMask: 0x3,
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}
