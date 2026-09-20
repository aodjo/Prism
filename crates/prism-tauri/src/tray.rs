//! The icon that outlives the window, and the reason closing one does not end Prism.
//!
//! A machine can be reached only while Prism is running on it, so closing the window has to
//! mean the window closed and nothing more. What stays behind is this: an icon — in the menu
//! bar on macOS, the notification area on Windows — that brings the window back, and the one
//! control that does end the application.
//!
//! Which makes it load-bearing rather than decorative. Every other way out of the application
//! was deliberately taken away, so an icon that fails to appear leaves somebody with a program
//! they can neither return to nor quit.

use tauri::{
    AppHandle, Manager,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};

use crate::windows;

/// Puts Prism in the menu bar.
///
/// # Errors
///
/// Fails if the item or its menu cannot be built. Reported rather than ignored, because without
/// it a closed window is the end of the application with nothing left to say so.
pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Prism", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Prism", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;

    let mut item = TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("Prism")
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => surface(app),
            "quit" => app.exit(0),
            _ => {}
        });

    if let Some(icon) = app.default_window_icon().cloned() {
        item = item.icon(icon);

        // Drawn from the icon's alpha rather than its colours, which is what the menu bar does
        // with everything else in it and what keeps one legible on a light bar and a dark one.
        //
        // macOS alone. Asking for it elsewhere hands the system an icon with its colours thrown
        // away and nothing that knows to put them back — a mark that is faint where it is not
        // invisible. And an invisible one here is not a cosmetic fault: this icon is the only
        // way back to a window that has been closed, and the only way to end the application,
        // so a person who cannot see it has a program they can neither reach nor quit.
        #[cfg(target_os = "macos")]
        {
            item = item.icon_as_template(true);
        }
    }

    item.build(app)?;

    Ok(())
}

/// Brings back the window that was closed, building it again if it is gone.
///
/// The order is the order somebody would expect to come back to: the window they were using,
/// then the one they would have been using, and only then a new one. Which of the two full-size
/// windows that is depends on how far through setup this machine has been, so an application
/// with none of them open opens whichever a launch would have.
pub fn surface(app: &AppHandle) {
    // Hiding the last window on macOS hides the application with it, and a window shown while
    // the application is hidden does not appear.
    #[cfg(target_os = "macos")]
    let _ = app.show();

    for label in ["home", "setup", "settings"] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.show();
            let _ = window.unminimize();
            let _ = window.set_focus();

            return;
        }
    }

    let (label, page) = crate::opening();
    let _ = windows::stage(app, label, page);
}
