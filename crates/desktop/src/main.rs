//! Asset Library desktop app: the library server in-process on a random localhost port,
//! shown in a native window, with a tray icon, native folder picker, save dialog and
//! drag-out. Closing the window keeps background work going; Quit is in the tray menu.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Arc;

use asset_library::api::AppState;
use asset_library::config::Config;
use asset_library::desktop::Desktop;
use asset_library::server;
use asset_library::settings::Settings;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::DialogExt;

const MAIN: &str = "main";

struct NativeDesktop {
    app: AppHandle,
}

impl Desktop for NativeDesktop {
    fn pick_folder(&self) -> Option<PathBuf> {
        let mut dialog = self.app.dialog().file();
        if let Some(w) = self.app.get_webview_window(MAIN) {
            dialog = dialog.set_parent(&w);
        }
        dialog.blocking_pick_folder()?.into_path().ok()
    }

    fn pick_save_file(&self, default_name: &str) -> Option<PathBuf> {
        let mut dialog = self.app.dialog().file().set_file_name(default_name).add_filter("Zip archive", &["zip"]);
        if let Some(w) = self.app.get_webview_window(MAIN) {
            dialog = dialog.set_parent(&w);
        }
        dialog.blocking_save_file()?.into_path().ok()
    }

    fn start_drag(&self, files: Vec<PathBuf>, icon_png: Vec<u8>) -> anyhow::Result<()> {
        let window = self.app.get_webview_window(MAIN).ok_or_else(|| anyhow::anyhow!("no window"))?;
        // Drags must start on the UI thread, while the mouse button is still down.
        self.app.run_on_main_thread(move || {
            let item = drag::DragItem::Files(files);
            let image = drag::Image::Raw(icon_png);
            let done = |_result: drag::DragResult, _pos: drag::CursorPosition| {};
            #[cfg(target_os = "linux")]
            let result = window
                .gtk_window()
                .map_err(|e| e.to_string())
                .and_then(|w| drag::start_drag(&w, item, image, done, Default::default()).map_err(|e| e.to_string()));
            #[cfg(not(target_os = "linux"))]
            let result = drag::start_drag(&window, item, image, done, Default::default()).map_err(|e| e.to_string());
            if let Err(e) = result {
                eprintln!("drag failed: {e}");
            }
        })?;
        Ok(())
    }
}

/// Whether a tray icon can be shown. Linux trays need a session D-Bus (WSL has none), and
/// without a tray, hiding the window on close would leave no way back.
fn tray_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        let uid = std::fs::metadata("/proc/self").map(|m| std::os::unix::fs::MetadataExt::uid(&m)).unwrap_or(0);
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() || std::path::Path::new(&format!("/run/user/{uid}/bus")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(MAIN) {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// The app identifier before release builds (io.github.patch85.asset-library); test installs
/// kept their library in a data dir named after it.
const OLD_IDENTIFIER: &str = "com.assetlibrary.desktop";

/// Move a library from the old identifier's data dir, once, if the new one doesn't exist yet.
/// Both live in the same parent folder on every OS, so a rename is enough.
fn adopt_old_data_dir(data_dir: &std::path::Path) {
    let Some(old) = data_dir.parent().map(|p| p.join(OLD_IDENTIFIER)) else { return };
    if old.is_dir()
        && !data_dir.exists()
        && let Err(e) = std::fs::rename(&old, data_dir)
    {
        eprintln!("couldn't move {} to {}: {e}", old.display(), data_dir.display());
    }
}

/// Data in the per-user app data dir unless `ASSET_LIBRARY_DATA` says otherwise.
fn config(app: &AppHandle) -> anyhow::Result<Config> {
    let mut cfg = Config::from_env();
    if std::env::var_os("ASSET_LIBRARY_DATA").is_none() {
        cfg.data_dir = app.path().app_data_dir()?;
        adopt_old_data_dir(&cfg.data_dir);
        if std::env::var_os("ASSET_LIBRARY_MODELS").is_none() {
            cfg.model_dir = cfg.data_dir.join("models");
        }
    }
    Ok(cfg)
}

/// The UI keeps preferences (sort, thumbnail size, zoom) in localStorage, which is per
/// origin, so use the same port every launch when it's free.
const PREFERRED_PORT: u16 = 47_651;

/// Serve the API on localhost from a background runtime; returns the port.
fn start_server(state: Arc<AppState>) -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", PREFERRED_PORT))
        .or_else(|_| std::net::TcpListener::bind(("127.0.0.1", 0)))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    std::thread::Builder::new().name("server".into()).spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
            if let Err(e) = server::serve(listener, state).await {
                eprintln!("server stopped: {e}");
            }
        });
    })?;
    Ok(port)
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let handle = app.handle().clone();
    let cfg = config(&handle)?;
    let opts = server::Options {
        runtime_dir: app.path().resource_dir().ok().map(|d| d.join("onnxruntime")),
        defaults: Settings::default(), // AI off until the user opts in
        desktop: Some(Arc::new(NativeDesktop { app: handle.clone() })),
    };
    let state = server::open(&cfg, opts)?;
    let port = start_server(Arc::clone(&state))?;

    let url = format!("http://127.0.0.1:{port}/").parse()?;
    WebviewWindowBuilder::new(app, MAIN, WebviewUrl::External(url))
        .title("Asset Library")
        .inner_size(1400.0, 900.0)
        .min_inner_size(720.0, 480.0)
        .build()?;

    if !tray_available() {
        return Ok(()); // closing the window quits instead (see main)
    }
    let show = MenuItem::with_id(app, "show", "Show Asset Library", true, None::<&str>)?;
    let rescan = MenuItem::with_id(app, "rescan", "Rescan folders", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &rescan, &PredefinedMenuItem::separator(app)?, &quit])?;
    let mut tray = TrayIconBuilder::with_id("tray")
        .tooltip("Asset Library")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "show" => show_main(app),
            "rescan" => {
                state.rescan_all();
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

/// WebKitGTK's GPU paths render nothing in VMs, WSL and on some NVIDIA drivers; fall back
/// unless the user chose otherwise.
#[cfg(target_os = "linux")]
fn webkit_workarounds() {
    let wsl = std::fs::read_to_string("/proc/version").is_ok_and(|v| v.to_lowercase().contains("microsoft"));
    let mut vars = vec![("WEBKIT_DISABLE_DMABUF_RENDERER", "1")];
    if wsl {
        vars.push(("LIBGL_ALWAYS_SOFTWARE", "1"));
    }
    for (k, v) in vars {
        if std::env::var_os(k).is_none() {
            // SAFETY: called first thing in main, before any other thread exists.
            unsafe { std::env::set_var(k, v) };
        }
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    webkit_workarounds();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .plugin(tauri_plugin_dialog::init())
        .setup(setup)
        .on_window_event(|window, event| {
            // Keep running in the tray so scans and analysis can finish.
            if let (WindowEvent::CloseRequested { api, .. }, true) = (event, tray_available()) {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building Asset Library");
    app.run(|app, event| {
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = event {
            show_main(app);
        }
        let _ = (app, event);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_the_old_data_dir_once() {
        let parent = tempfile::tempdir().unwrap();
        let (old, new) = (parent.path().join(OLD_IDENTIFIER), parent.path().join("io.github.patch85.asset-library"));
        std::fs::create_dir_all(old.join("thumbs")).unwrap();
        std::fs::write(old.join("library.db"), b"db").unwrap();
        adopt_old_data_dir(&new);
        assert!(!old.exists() && std::fs::read(new.join("library.db")).unwrap() == b"db");

        // An existing new library is never replaced.
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("library.db"), b"stale").unwrap();
        adopt_old_data_dir(&new);
        assert_eq!(std::fs::read(new.join("library.db")).unwrap(), b"db");
        assert!(old.exists());
    }
}
