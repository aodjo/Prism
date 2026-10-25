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

#[cfg(target_os = "macos")]
use tauri::menu::MenuItemKind;

use crate::windows;
use prism_stream::locale::text;

/// Native menu items retained so a saved language change can update them.
struct Labels {
    open: MenuItem<tauri::Wry>,
    quit: MenuItem<tauri::Wry>,
}

/// Puts Prism in the menu bar.
///
/// # Errors
///
/// Fails if the item or its menu cannot be built. Reported rather than ignored, because without
/// it a closed window is the end of the application with nothing left to say so.
pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(
        app,
        "open",
        text("Open PRISM", "PRISM 열기"),
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(
        app,
        "quit",
        text("Quit PRISM", "PRISM 종료"),
        true,
        None::<&str>,
    )?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    app.manage(Labels { open, quit });

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
    application_menu(app)?;

    Ok(())
}

/// Updates existing menu items after the language preference has been saved.
///
/// # Errors
///
/// Fails if the platform menu cannot update its labels.
pub fn relabel(app: &AppHandle) -> tauri::Result<()> {
    if let Some(labels) = app.try_state::<Labels>() {
        labels.open.set_text(text("Open PRISM", "PRISM 열기"))?;
        labels.quit.set_text(text("Quit PRISM", "PRISM 종료"))?;
    }
    application_menu(app)?;
    Ok(())
}

/// Rebuilds the standard application menus in the selected language while keeping native actions.
#[cfg(target_os = "macos")]
fn application_menu(app: &AppHandle) -> tauri::Result<()> {
    let menu = Menu::default(app)?;
    translate_items(menu.items()?)?;
    app.set_menu(menu)?;
    Ok(())
}

/// Leaves platforms without the standard macOS application menu unchanged.
#[cfg(not(target_os = "macos"))]
fn application_menu(_app: &AppHandle) -> tauri::Result<()> {
    Ok(())
}

/// Translates standard menu labels recursively, including their native predefined actions.
#[cfg(target_os = "macos")]
fn translate_items(items: Vec<MenuItemKind<tauri::Wry>>) -> tauri::Result<()> {
    for item in items {
        match item {
            MenuItemKind::Submenu(menu) => {
                menu.set_text(menu_label(&menu.text()?))?;
                translate_items(menu.items()?)?;
            }
            MenuItemKind::Predefined(item) => item.set_text(menu_label(&item.text()?))?,
            _ => {}
        }
    }
    Ok(())
}

/// Returns Korean copy for a standard menu label, retaining unknown platform labels.
#[cfg(target_os = "macos")]
fn menu_label(english: &str) -> &str {
    let korean = match english {
        "File" => "파일",
        "Edit" => "편집",
        "View" => "보기",
        "Window" => "윈도우",
        "Help" => "도움말",
        "About Prism" | "About" => "PRISM 정보",
        "Services" => "서비스",
        "Hide Prism" | "Hide" => "PRISM 가리기",
        "Hide Others" => "기타 가리기",
        "Show All" => "모두 보기",
        "Quit Prism" | "Quit" => "PRISM 종료",
        "Close Window" | "Close" => "윈도우 닫기",
        "Undo" => "실행 취소",
        "Redo" => "다시 실행",
        "Cut" => "오려두기",
        "Copy" => "복사",
        "Paste" => "붙여넣기",
        "Select All" => "모두 선택",
        "Minimize" => "최소화",
        "Maximize" | "Zoom" => "확대/축소",
        "Enter Full Screen" | "Toggle Full Screen" | "Fullscreen" => "전체 화면",
        _ => english,
    };
    text(english, korean)
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

    // Built from a thread of its own. This runs inside an event handler, which is the main
    // thread, and on Windows a web view cannot finish being made while that thread is held —
    // the window arrives black and cannot be closed. The same trap the window commands are
    // kept out of, reached by a different door.
    let (label, page) = crate::opening();
    let app = app.clone();

    std::thread::spawn(move || {
        let _ = windows::stage(&app, label, page);
    });
}
