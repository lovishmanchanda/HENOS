//! HENOS for Windows and macOS.
//!
//! This shell is deliberately thin. Everything that matters lives in
//! `henos-app`; here we only connect it to a window, a tray icon and the
//! system clipboard.

// No console window behind the app on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{path::PathBuf, sync::Arc};

use henos_app::{App as Henos, AppConfig, AppState, PairedView, PairingView};
use henos_clipboard_desktop::{ClipboardWatch, SystemClipboard};
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager, RunEvent, State, WindowEvent, Wry,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as _};
use tracing::{error, info};

/// Passed when HENOS is started at login, so it opens without a window.
const HIDDEN_FLAG: &str = "--hidden";

/// Event sent to the window whenever it should fetch a fresh state.
const STATE_CHANGED: &str = "state-changed";

/// What the running app keeps for its commands and menus.
struct Running {
    henos: Arc<Henos>,
    pause_item: CheckMenuItem<Wry>,
    /// Stops watching the clipboard when dropped.
    _watch: ClipboardWatch,
}

#[tauri::command]
async fn get_state(running: State<'_, Running>) -> Result<AppState, String> {
    // Logged once: confirms the window loaded and can reach the Rust side.
    static FIRST: std::sync::Once = std::sync::Once::new();
    FIRST.call_once(|| info!("window connected"));
    Ok(running.henos.state())
}

#[tauri::command]
async fn start_pairing(running: State<'_, Running>) -> Result<PairingView, String> {
    running.henos.start_pairing()
}

#[tauri::command]
async fn stop_pairing(running: State<'_, Running>) -> Result<(), String> {
    running.henos.stop_pairing();
    Ok(())
}

#[tauri::command]
async fn pair_with_code(
    running: State<'_, Running>,
    device_id: String,
    code: String,
) -> Result<PairedView, String> {
    running.henos.pair_with_code(&device_id, &code).await
}

#[tauri::command]
async fn pair_with_ticket(
    running: State<'_, Running>,
    ticket: String,
) -> Result<PairedView, String> {
    running.henos.pair_with_ticket(&ticket).await
}

#[tauri::command]
async fn unpair(running: State<'_, Running>, device_id: String) -> Result<(), String> {
    running.henos.unpair(&device_id)
}

#[tauri::command]
async fn set_clipboard_paused(running: State<'_, Running>, paused: bool) -> Result<(), String> {
    running.henos.set_clipboard_paused(paused);
    // Keep the tray checkbox in step with the window.
    let _ = running.pause_item.set_checked(paused);
    Ok(())
}

/// Where this install keeps its key and paired-device list. `HENOS_DATA_DIR`
/// overrides the system location, for development and for running a second
/// instance on one computer.
fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("HENOS_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }
    app.path()
        .app_data_dir()
        .map_err(|e| format!("Could not find a folder to keep HENOS's data in: {e}"))
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Starts HENOS and wires it to the clipboard, the window and the tray.
fn start(app: &AppHandle) -> Result<(), String> {
    let mut config = AppConfig::new(data_dir(app)?);
    config.device_name = std::env::var("HENOS_DEVICE_NAME").ok();

    let clipboard = Arc::new(SystemClipboard::new()?);
    let henos = tauri::async_runtime::block_on(Henos::start(config, clipboard))?;
    let me = henos.state().device;
    info!(name = %me.name, id = %me.id, "HENOS started");

    // Copies made on this computer go out to paired devices.
    let copier = henos.clone();
    let watch = henos_clipboard_desktop::watch(move |content| {
        let copier = copier.clone();
        tauri::async_runtime::spawn(async move { copier.local_copy(content).await });
    })?;

    // Tell the window whenever there is something new to show.
    let mut changes = henos.subscribe_changes();
    let notifier = app.clone();
    tauri::async_runtime::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        // Missing a few notices is fine: one refresh covers them all.
        while let Ok(()) | Err(RecvError::Lagged(_)) = changes.recv().await {
            let _ = notifier.emit(STATE_CHANGED, ());
        }
    });

    let pause_item = build_tray(app).map_err(|e| format!("Could not create the tray icon: {e}"))?;
    app.manage(Running {
        henos,
        pause_item,
        _watch: watch,
    });
    Ok(())
}

/// Creates the tray icon and its menu. Returns the pause checkbox so the
/// window can keep it in step.
fn build_tray(app: &AppHandle) -> tauri::Result<CheckMenuItem<Wry>> {
    let open = MenuItem::with_id(app, "open", "Open HENOS", true, None::<&str>)?;
    let pause = CheckMenuItem::with_id(
        app,
        "pause",
        "Pause clipboard sharing",
        true,
        false,
        None::<&str>,
    )?;
    // Off until the person turns it on: HENOS does not add itself to the
    // login items on its own.
    let autostart_on = app.autolaunch().is_enabled().unwrap_or(false);
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "Start HENOS at login",
        true,
        autostart_on,
        None::<&str>,
    )?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit HENOS", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &pause, &autostart, &separator, &quit])?;
    let autostart_item = autostart.clone();

    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("HENOS")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "open" => show_main_window(app),
            "pause" => {
                let running = app.state::<Running>();
                let paused = !running.henos.clipboard_paused();
                running.henos.set_clipboard_paused(paused);
                let _ = running.pause_item.set_checked(paused);
            }
            "autostart" => {
                let launcher = app.autolaunch();
                let enable = !launcher.is_enabled().unwrap_or(false);
                let changed = if enable {
                    launcher.enable()
                } else {
                    launcher.disable()
                };
                if let Err(e) = changed {
                    error!("could not change the start-at-login setting: {e}");
                }
                // Show what is actually set, whether or not the change worked.
                let _ = autostart_item.set_checked(launcher.is_enabled().unwrap_or(false));
            }
            "quit" => quit_app(app),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(pause)
}

/// Closes connections cleanly, then exits.
fn quit_app(app: &AppHandle) {
    if let Some(running) = app.try_state::<Running>() {
        tauri::async_runtime::block_on(running.henos.shutdown());
    }
    app.exit(0);
}

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(if cfg!(debug_assertions) {
            tracing::Level::INFO
        } else {
            tracing::Level::WARN
        })
        .with_writer(std::io::stderr)
        .init();

    let app = tauri::Builder::default()
        // Opening HENOS a second time brings up the window of the copy that
        // is already running instead of starting another one. Must be the
        // first plugin registered.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        // When started at login HENOS gets this flag and stays in the tray.
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![HIDDEN_FLAG]),
        ))
        .setup(|app| {
            if let Err(e) = start(app.handle()) {
                error!("HENOS could not start: {e}");
                return Err(e.into());
            }
            // The window is created hidden; show it unless we were started
            // at login.
            if !std::env::args().any(|arg| arg == HIDDEN_FLAG) {
                show_main_window(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps HENOS running in the tray, so the
            // clipboard stays shared. Quit from the tray menu.
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            start_pairing,
            stop_pairing,
            pair_with_code,
            pair_with_ticket,
            unpair,
            set_clipboard_paused,
        ])
        .build(tauri::generate_context!())
        .expect("error while building the HENOS app");

    app.run(|app, event| match event {
        // macOS: clicking the dock icon brings the window back.
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => show_main_window(app),
        RunEvent::ExitRequested {
            code: None, api, ..
        } => {
            // The last window closing is not a reason to quit.
            let _ = app;
            api.prevent_exit();
        }
        _ => {}
    });
}
