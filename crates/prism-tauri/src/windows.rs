//! The windows there are, and moving between them.
//!
//! Three, and each is a different question. Setup asks the questions somebody answers once; home
//! is what the application is for; and the settings are a narrow panel beside it rather than a
//! page inside it, because settings are a detour and a detour that hides what it interrupts is
//! one people lose their place in.
//!
//! None of them draws a stream. That is a window this process does not own and never sees.

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

/// What the setup flow and the home window open at.
const STAGE: (f64, f64) = (1280.0, 840.0);

/// The narrowest either of them goes before the design stops fitting.
const STAGE_FLOOR: (f64, f64) = (1040.0, 720.0);

/// How wide the settings panel is. It holds a list and a few rows, and does not resize.
const PANEL_WIDTH: f64 = 420.0;

/// The shortest the panel goes, so a failed render is not an invisible window.
const PANEL_FLOOR: f64 = 220.0;

/// The tallest it goes, so a long list of machines does not fill the screen.
const PANEL_CEILING: f64 = 760.0;

/// The colour behind the page while it loads.
///
/// Stated on every window rather than only the large ones. The design is dark throughout, and a
/// window that flashes white before its first paint is the one thing a person always notices.
const BASE: tauri::window::Color = tauri::window::Color(8, 8, 11, 255);

/// Opens one of the two full-size windows, or raises it if it is already there.
///
/// # Errors
///
/// Fails if the window cannot be built.
pub fn stage(app: &AppHandle, label: &str, page: &str) -> tauri::Result<WebviewWindow> {
    if let Some(open) = app.get_webview_window(label) {
        open.set_focus()?;

        return Ok(open);
    }

    let window = WebviewWindowBuilder::new(app, label, WebviewUrl::App(page.into()))
        .title("Prism")
        .inner_size(STAGE.0, STAGE.1)
        .min_inner_size(STAGE_FLOOR.0, STAGE_FLOOR.1)
        .center()
        .background_color(BASE);

    let window = overlaid(window).build()?;
    let hiding = window.clone();

    // Closing the window closes the window. This machine is shareable for exactly as long as
    // Prism runs on it, so ending the application is something asked for in the menu bar and
    // not something that happens because somebody was done looking at a list of machines.
    //
    // Which is why the two places that mean it use `destroy`: this fires for a close asked for
    // in code as readily as for one asked for with the mouse, and neither of them wants a
    // window that comes back.
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();

            let _ = hiding.hide();
        }
    });

    Ok(window)
}

/// Puts the window's content where its title bar would be, where that is a thing windows do.
///
/// The design draws its own header and carries the traffic lights over the top left of it, and
/// it names the window in the markup — so a second name printed over that by the system is the
/// header with a title bar on top of it.
///
/// Both of these are macOS's, and the builder does not have them anywhere else. Elsewhere the
/// window keeps the frame its system draws, which is what somebody there expects a window to
/// look like.
fn overlaid<R: tauri::Runtime, M: tauri::Manager<R>>(
    builder: WebviewWindowBuilder<'_, R, M>,
) -> WebviewWindowBuilder<'_, R, M> {
    #[cfg(target_os = "macos")]
    let builder = builder
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true);

    builder
}

/// Shows the home window and closes setup, which is what finishing setup means.
///
/// # Errors
///
/// Fails if the home window cannot be built.
pub fn open_home(app: &AppHandle) -> tauri::Result<()> {
    stage(app, "home", "home.html")?;

    if let Some(setup) = app.get_webview_window("setup") {
        setup.destroy()?;
    }

    Ok(())
}

/// Shows setup and closes the home window.
///
/// The counterpart of [`open_home`], and what signing out does. Without it somebody who signs
/// out is left looking at a window built entirely out of what the account said — a list of
/// machines nobody can reach any more and a name that came from an account this machine has
/// just left.
///
/// # Errors
///
/// Fails if the setup window cannot be built.
pub fn open_setup(app: &AppHandle) -> tauri::Result<()> {
    stage(app, "setup", "setup.html")?;

    if let Some(home) = app.get_webview_window("home") {
        home.destroy()?;
    }

    Ok(())
}

/// Opens the settings panel, or raises it if it is already open.
///
/// # Errors
///
/// Fails if the window cannot be built.
#[tauri::command]
pub fn open_settings(app: AppHandle) -> Result<(), String> {
    if let Some(open) = app.get_webview_window("settings") {
        return open.set_focus().map_err(|error| error.to_string());
    }

    let panel = WebviewWindowBuilder::new(&app, "settings", WebviewUrl::App("index.html".into()))
        .title("Prism")
        .inner_size(PANEL_WIDTH, PANEL_FLOOR)
        // It is as tall as what is in it and no wider than one column, so there is nothing for
        // dragging an edge or filling the screen to achieve.
        .resizable(false)
        .maximizable(false)
        .background_color(BASE);

    overlaid(panel)
        .build()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Makes the settings panel as tall as what is in it.
///
/// The window measures its own content and says so, because only the page knows: the account
/// section changes height by a form's worth when somebody signs in, and a height decided here
/// would be a guess that is wrong half the time.
///
/// # Errors
///
/// Fails if the window cannot be resized. A call arriving when the panel is not open is not a
/// failure — it is a page that was closed between measuring itself and saying so.
#[tauri::command]
pub fn fit(height: f64, app: AppHandle) -> Result<(), String> {
    let Some(panel) = app.get_webview_window("settings") else {
        return Ok(());
    };

    let wanted = height.clamp(PANEL_FLOOR, PANEL_CEILING).round();

    panel
        .set_size(tauri::LogicalSize::new(PANEL_WIDTH, wanted))
        .map_err(|error| error.to_string())
}

/// Closes setup and opens the home window.
///
/// Writes down that setup has been reached the end of, which is what the next launch reads. The
/// permissions step cannot be finished in one sitting — macOS reads a new grant only when the
/// application starts again — so without this the restart in the middle of setup looks like an
/// ordinary launch and lands on the home window instead of back where somebody was.
///
/// # Errors
///
/// Fails if the settings lock was poisoned, if they cannot be written, or if the home window
/// cannot be built.
#[tauri::command]
pub fn finish_setup(app: AppHandle, held: tauri::State<'_, crate::Held>) -> Result<(), String> {
    {
        let mut settings = held
            .0
            .lock()
            .map_err(|_| "the settings lock was poisoned".to_owned())?;

        settings.setup_finished = true;
        crate::settings::save(&settings)?;
    }

    open_home(&app).map_err(|error| error.to_string())
}
