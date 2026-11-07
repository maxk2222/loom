//! Optional system-tray icon for the client daemon (macOS/Windows/Linux).
//!
//! Runs on the main thread on macOS (AppKit requirement) — call `run()`
//! from `main` AFTER the tokio runtime has been moved to a worker thread.
//! On macOS the AppKit run loop must be pumped or menu clicks never fire.

use anyhow::{Context, Result};

const ICON_PNG: &[u8] = include_bytes!("../assets/tray-icon.png");
const UI_URL: &str = "http://127.0.0.1:8391/ui/";

fn load_icon() -> Result<tray_icon::Icon> {
    let img = image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png)
        .context("decode tray png")?
        .into_rgba8();
    let (w, h) = img.dimensions();
    tray_icon::Icon::from_rgba(img.into_raw(), w, h).context("tray icon")
}

/// Open the local admin UI in the default browser (platform-specific).
fn open_ui() {
    let url = std::env::var("LOOM_UI_URL").unwrap_or_else(|_| UI_URL.into());
    let _ = std::process::Command::new(if cfg!(target_os = "windows") {
        "cmd"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    })
    .args(if cfg!(target_os = "windows") {
        vec!["/c", "start", "", url.as_str()]
    } else {
        vec![url.as_str()]
    })
    .spawn();
}

fn handle_menu(
    ev: tray_icon::menu::MenuEvent,
    open_id: &tray_icon::menu::MenuId,
    quit_id: &tray_icon::menu::MenuId,
) {
    if ev.id == *open_id {
        open_ui();
    } else if ev.id == *quit_id {
        std::process::exit(0);
    }
}

/// Blocking tray event loop — must run on the main thread (macOS).
/// Returns when the user picks Quit.
pub fn run() -> Result<()> {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{TrayIconBuilder, TrayIconEvent};

    let icon = load_icon()?;
    let open_item = MenuItem::new("Open LOOM UI", true, None);
    let quit_item = MenuItem::new("Quit", true, None);
    let menu = Menu::with_items(&[&open_item as &dyn tray_icon::menu::IsMenuItem, &quit_item])?;
    let open_id = open_item.id().clone();
    let quit_id = quit_item.id().clone();
    drop(open_item);
    drop(quit_item);

    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("LOOM Client")
        .with_icon(icon)
        .build()
        .context("build tray")?;

    let menu_rx = MenuEvent::receiver();
    let tray_rx = TrayIconEvent::receiver();

    #[cfg(target_os = "macos")]
    {
        // A bare binary has no NSApplication — without one, the status item
        // renders but its menu never opens on click. Initialize as a menu-bar
        // agent app (Accessory: no Dock icon, no Cmd-Tab entry).
        use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
        use objc2_foundation::MainThreadMarker;
        let mtm = MainThreadMarker::new().expect("tray must run on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        // Menu clicks are delivered through the app run loop — pump it in
        // short bursts so our channel polling stays responsive.
        use core_foundation::runloop::{kCFRunLoopDefaultMode, CFRunLoopRunInMode};
        loop {
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.1, 1) };
            if let Ok(ev) = tray_rx.try_recv() {
                if matches!(ev, TrayIconEvent::DoubleClick { .. }) {
                    open_ui();
                }
            }
            if let Ok(ev) = menu_rx.try_recv() {
                handle_menu(ev, &open_id, &quit_id);
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    loop {
        if let Ok(ev) = tray_rx.try_recv() {
            if matches!(ev, TrayIconEvent::DoubleClick { .. }) {
                open_ui();
            }
        }
        if let Ok(ev) = menu_rx.try_recv() {
            handle_menu(ev, &open_id, &quit_id);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
