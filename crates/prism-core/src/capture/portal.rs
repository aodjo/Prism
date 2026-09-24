//! Asking the desktop for its screen and its input, on Linux.
//!
//! A Wayland desktop gives neither to an application that simply asks the display server: no
//! client can read another's pixels or move a pointer it does not own. What it offers instead is
//! the portal — `xdg-desktop-portal`, a service the compositor ships — which puts the question to
//! the person at the machine and, if they agree, hands back a PipeWire stream of the screen and
//! a way to inject pointer and keyboard events.
//!
//! # One question, asked once
//!
//! The screen and the input are asked for together, in one Remote Desktop session with the
//! screen cast attached, so the person at the machine is shown one dialog rather than two. It
//! is asked with *persist until revoked*: the portal answers with a restore token, kept on disk
//! beside this machine's key, and a session opened with that token is granted without a dialog.
//! So the question appears the first time this machine is shared — see [`grant`], which the shell
//! calls then, while somebody is there to answer it — and not again, until somebody revokes it in
//! their desktop's settings.
//!
//! A token is good for one session: every start hands back a new one, and the old one stops
//! working. So the token is replaced each time, not merely read.
//!
//! # A desktop that offers only the screen
//!
//! Some compositors implement the screen cast portal and not the remote desktop one. There this
//! falls back to the screen alone, persisted the same way, and the machine can be watched but not
//! controlled — which is what [`input`] saying nothing means to the injector.
//!
//! # Blocking on purpose
//!
//! The portal is D-Bus, and the library that speaks it is asynchronous. Nothing here is on the
//! frame path, so each call is simply waited for on the thread that made it — no runtime, and in
//! particular not the one the hot path is forbidden. The injection calls are the exception worth
//! naming: each is a D-Bus round trip, a fraction of a millisecond, made on the thread that reads
//! the client's input rather than on the one that sends frames.

#![cfg(linux_desktop)]

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ashpd::desktop::remote_desktop::{
    Axis, DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions,
};
use ashpd::desktop::screencast::{
    CursorMode, Screencast, SelectSourcesOptions, SourceType, Stream,
};
use ashpd::desktop::{PersistMode, Session};
use ashpd::enumflags2::BitFlags;

use crate::capture::CaptureError;

/// How long the person at the machine is given to answer the portal's dialog.
///
/// A minute: long enough to walk over to a machine that has just been shared, short enough that
/// a dialog nobody is going to answer does not hold the host's thread for good. A session that
/// was restored from a token asks nothing and returns at once.
const ANSWER_PATIENCE: Duration = Duration::from_secs(60);

/// The desktop's permission, held for as long as a session is using it.
///
/// Dropping it closes the portal session, which ends the screen cast and takes away the input,
/// and stops anything still holding [`input`] from reaching the desktop.
pub struct Granted {
    /// The PipeWire remote the screen is on, until it is taken to connect to.
    remote: Option<OwnedFd>,
    /// The PipeWire node that is the screen.
    pub node: u32,
    /// The screen's size in the desktop's logical units, where the portal said.
    pub size: Option<(u32, u32)>,
    session: Held,
}

impl Granted {
    /// Takes the connection to the PipeWire remote the screen is on.
    ///
    /// Once: a PipeWire context takes ownership of the descriptor it connects through.
    pub fn take_remote(&mut self) -> Option<OwnedFd> {
        self.remote.take()
    }

    /// Whether the desktop granted input as well as the screen.
    #[must_use]
    pub fn controls(&self) -> bool {
        matches!(self.session, Held::Remote(_))
    }
}

impl Drop for Granted {
    /// Closes the portal session, and with it the screen and the input.
    fn drop(&mut self) {
        if let Ok(mut slot) = INPUT.lock() {
            *slot = None;
        }

        match &self.session {
            Held::Remote(input) => {
                let _ = pollster::block_on(input.session.close());
            }
            Held::Cast { session, .. } => {
                let _ = pollster::block_on(session.close());
            }
        }
    }
}

impl core::fmt::Debug for Granted {
    /// Names what was granted, not the D-Bus objects behind it.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Granted")
            .field("node", &self.node)
            .field("size", &self.size)
            .field("controls", &self.controls())
            .finish_non_exhaustive()
    }
}

/// Which of the two kinds of session is open.
enum Held {
    /// Screen and input together, the session held where the injector can reach it.
    Remote(Arc<Input>),
    /// The screen alone, on a desktop without the remote desktop portal.
    Cast {
        /// Kept for the same reason.
        _proxy: Screencast,
        session: Session<Screencast>,
    },
}

/// The way into the desktop's input, while a session holds it.
///
/// Shared rather than owned because the two things that need the session are on different
/// threads: the capture opens it, and the injector — started first, on the thread that reads the
/// client — sends through it. The proxy is kept with the session because the session belongs to
/// the bus connection that proxy made.
pub struct Input {
    proxy: RemoteDesktop,
    session: Session<RemoteDesktop>,
    node: u32,
    size: (f64, f64),
}

impl Input {
    /// Moves the pointer by an amount.
    ///
    /// # Errors
    ///
    /// Whatever the portal says, as a message.
    pub fn move_by(&self, dx: f64, dy: f64) -> Result<(), String> {
        pollster::block_on(self.proxy.notify_pointer_motion(
            &self.session,
            dx,
            dy,
            Default::default(),
        ))
        .map_err(|err| err.to_string())
    }

    /// Puts the pointer a fraction of the way across and down the screen being shared.
    ///
    /// # Errors
    ///
    /// Whatever the portal says, as a message.
    pub fn move_to(&self, across: f64, down: f64) -> Result<(), String> {
        pollster::block_on(self.proxy.notify_pointer_motion_absolute(
            &self.session,
            self.node,
            across.clamp(0.0, 1.0) * self.size.0,
            down.clamp(0.0, 1.0) * self.size.1,
            Default::default(),
        ))
        .map_err(|err| err.to_string())
    }

    /// Presses or releases a pointer button, named by its evdev code.
    ///
    /// # Errors
    ///
    /// Whatever the portal says, as a message.
    pub fn button(&self, code: i32, pressed: bool) -> Result<(), String> {
        pollster::block_on(self.proxy.notify_pointer_button(
            &self.session,
            code,
            state(pressed),
            Default::default(),
        ))
        .map_err(|err| err.to_string())
    }

    /// Turns the wheel by whole notches, down and right being positive.
    ///
    /// # Errors
    ///
    /// Whatever the portal says, as a message.
    pub fn scroll(&self, across: i32, down: i32) -> Result<(), String> {
        for (axis, steps) in [(Axis::Vertical, down), (Axis::Horizontal, across)] {
            if steps != 0 {
                pollster::block_on(self.proxy.notify_pointer_axis_discrete(
                    &self.session,
                    axis,
                    steps,
                    Default::default(),
                ))
                .map_err(|err| err.to_string())?;
            }
        }

        Ok(())
    }

    /// Presses or releases a key, named by its evdev code.
    ///
    /// # Errors
    ///
    /// Whatever the portal says, as a message.
    pub fn key(&self, code: i32, pressed: bool) -> Result<(), String> {
        pollster::block_on(self.proxy.notify_keyboard_keycode(
            &self.session,
            code,
            state(pressed),
            Default::default(),
        ))
        .map_err(|err| err.to_string())
    }
}

impl core::fmt::Debug for Input {
    /// Names the stream the input is aimed at.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Input")
            .field("node", &self.node)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// The input of the session that is open, if one is and it was granted input.
static INPUT: Mutex<Option<Arc<Input>>> = Mutex::new(None);

/// The way into the desktop's input, if a session is holding one.
#[must_use]
pub fn input() -> Option<Arc<Input>> {
    INPUT.lock().ok()?.clone()
}

/// Opens a session with the desktop, restored from the last one if it can be.
///
/// Waits for the person at the machine if the portal asks them, up to a minute.
///
/// # Errors
///
/// Returns [`CaptureError::PermissionDenied`] if they refused or nobody answered, and
/// [`CaptureError::Start`] if there is no portal to ask or it failed.
pub fn open() -> Result<Granted, CaptureError> {
    let _asking = ASKING.lock();

    ask_and_wait()
}

/// Asks once, and closes again, so that later sessions open without asking.
///
/// For the shell to call when this machine is shared, which is the moment somebody is sitting at
/// it to answer. Does nothing if a token is already kept.
///
/// # Errors
///
/// Whatever [`open`] would.
pub fn grant() -> Result<(), CaptureError> {
    let _asking = ASKING.lock();

    if kept_token().is_some() {
        return Ok(());
    }

    drop(ask_and_wait()?);

    Ok(())
}

/// Held by whoever is asking the portal, so that two askers do not raise two dialogs.
///
/// Sharing being switched on asks in the background; somebody connecting a moment later asks
/// again to open the session. Without this the second would find no token yet and ask the person
/// at the machine a second time — and whichever answer came last would be the token kept.
static ASKING: Mutex<()> = Mutex::new(());

/// Opens a session, waiting for an answer if the portal asks for one.
fn ask_and_wait() -> Result<Granted, CaptureError> {
    let (answer, answered) = mpsc::channel();

    // On a thread of its own so that a dialog nobody answers is a thread left waiting rather
    // than a host that can never stop sharing. The portal dismisses the dialog itself when the
    // session it belongs to goes, which is when this process does.
    std::thread::Builder::new()
        .name("prism-portal".into())
        .spawn(move || {
            let _ = answer.send(pollster::block_on(ask()));
        })
        .map_err(|err| CaptureError::Start {
            reason: err.to_string(),
        })?;

    let granted = answered
        .recv_timeout(ANSWER_PATIENCE)
        .map_err(|_| CaptureError::PermissionDenied)??;

    if let Held::Remote(input) = &granted.session
        && let Ok(mut slot) = INPUT.lock()
    {
        *slot = Some(Arc::clone(input));
    }

    Ok(granted)
}

/// Opens the session: remote desktop with the screen attached where the desktop has both, the
/// screen alone where it does not.
async fn ask() -> Result<Granted, CaptureError> {
    let token = kept_token();
    let screencast = Screencast::new().await.map_err(start)?;

    // Embedded where the desktop can, so the pointer is in the picture. This end has no other
    // way to say where it is: a Wayland client cannot read the pointer's position, so the
    // position the other platforms send beside the picture has nothing to be read from here.
    // Not metadata: that puts the position in the stream beside each frame, and nothing here
    // reads it yet, so asking for it would be asking for a picture with no pointer at all.
    let cursor = match screencast.available_cursor_modes().await {
        Ok(modes) if modes.contains(CursorMode::Embedded) => CursorMode::Embedded,
        _ => CursorMode::Hidden,
    };

    let sources = SelectSourcesOptions::default()
        .set_cursor_mode(cursor)
        .set_sources(BitFlags::from(SourceType::Monitor))
        .set_multiple(false);

    if let Ok(remote) = RemoteDesktop::new().await {
        let session = remote
            .create_session(Default::default())
            .await
            .map_err(start)?;

        remote
            .select_devices(
                &session,
                SelectDevicesOptions::default()
                    .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
                    .set_restore_token(token.as_deref()),
            )
            .await
            .map_err(start)?;

        // The persistence belongs to the remote desktop session here, and the portal refuses a
        // screen cast that asks for it as well.
        screencast
            .select_sources(&session, sources)
            .await
            .map_err(start)?;

        let started = remote
            .start(&session, None, Default::default())
            .await
            .map_err(start)?
            .response()
            .map_err(refused)?;

        keep_token(started.restore_token());

        let stream = first(started.streams())?;
        let remote_fd = screencast
            .open_pipe_wire_remote(&session, Default::default())
            .await
            .map_err(start)?;
        let size = logical_size(&stream);

        return Ok(Granted {
            remote: Some(remote_fd),
            node: stream.pipe_wire_node_id(),
            size,
            session: Held::Remote(Arc::new(Input {
                proxy: remote,
                session,
                node: stream.pipe_wire_node_id(),
                size: size.map_or((1.0, 1.0), |(w, h)| (f64::from(w), f64::from(h))),
            })),
        });
    }

    let session = screencast
        .create_session(Default::default())
        .await
        .map_err(start)?;

    screencast
        .select_sources(
            &session,
            sources
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token.as_deref()),
        )
        .await
        .map_err(start)?;

    let started = screencast
        .start(&session, None, Default::default())
        .await
        .map_err(start)?
        .response()
        .map_err(refused)?;

    keep_token(started.restore_token());

    let stream = first(started.streams())?;
    let remote_fd = screencast
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(start)?;

    Ok(Granted {
        remote: Some(remote_fd),
        node: stream.pipe_wire_node_id(),
        size: logical_size(&stream),
        session: Held::Cast {
            _proxy: screencast,
            session,
        },
    })
}

/// The one stream a monitor-only, single-source session returns.
fn first(streams: &[Stream]) -> Result<Stream, CaptureError> {
    streams.first().cloned().ok_or(CaptureError::NoDisplay)
}

/// The stream's size in logical units, which is what absolute pointer positions are given in.
fn logical_size(stream: &Stream) -> Option<(u32, u32)> {
    let (width, height) = stream.size()?;

    Some((u32::try_from(width).ok()?, u32::try_from(height).ok()?))
}

/// A key or button state as the portal spells it.
fn state(pressed: bool) -> KeyState {
    if pressed {
        KeyState::Pressed
    } else {
        KeyState::Released
    }
}

/// A portal failure, as a capture that could not start.
fn start(err: ashpd::Error) -> CaptureError {
    CaptureError::Start {
        reason: format!("the desktop portal: {err}"),
    }
}

/// A dialog that came back with anything but yes.
fn refused(err: ashpd::Error) -> CaptureError {
    match err {
        ashpd::Error::Response(_) => CaptureError::PermissionDenied,
        other => start(other),
    }
}

/// Where the restore token is kept: beside this machine's key.
fn token_path() -> Option<PathBuf> {
    Some(
        crate::identity::default_path()
            .ok()?
            .with_file_name("portal-token"),
    )
}

/// The token the last session was granted, if one was kept.
fn kept_token() -> Option<String> {
    let text = std::fs::read_to_string(token_path()?).ok()?;
    let token = text.trim();

    (!token.is_empty()).then(|| token.to_owned())
}

/// Keeps the token a session was granted, replacing the one it was opened with.
///
/// A session granted none — the desktop does not persist, or the person chose not to let it —
/// removes the old one, which no longer works either way.
fn keep_token(token: Option<&str>) {
    let Some(path) = token_path() else {
        return;
    };

    match token {
        Some(token) => {
            if let Some(folder) = path.parent() {
                let _ = std::fs::create_dir_all(folder);
            }

            let _ = crate::store::replace(&path, token.as_bytes());
        }
        None => {
            let _ = std::fs::remove_file(path);
        }
    }
}
