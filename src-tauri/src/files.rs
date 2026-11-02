//! Reading and writing the user's files.
//!
//! On Android, file pickers, "Open with" and "Share" hand over `content://` URIs instead
//! of paths. Only the system can resolve them, so those go through the fs plugin and a
//! small Kotlin plugin (`FilesPlugin.kt`); everything else uses `std::fs`.

use std::path::Path;

/// True for Android `content://` URIs.
pub fn is_uri(path: &str) -> bool {
    path.starts_with("content://")
}

pub fn read(path: &str) -> Result<Vec<u8>, String> {
    #[cfg(target_os = "android")]
    if is_uri(path) {
        return android::read(path);
    }
    std::fs::read(path).map_err(|e| format!("no se pudo leer {}: {e}", display_name(path)))
}

pub fn write(path: &str, bytes: &[u8]) -> Result<(), String> {
    #[cfg(target_os = "android")]
    if is_uri(path) {
        return android::write(path, bytes);
    }
    std::fs::write(path, bytes).map_err(|e| format!("no se pudo guardar {}: {e}", display_name(path)))
}

/// Name to show for a file ("factura.pdf"), also for URIs.
pub fn display_name(path: &str) -> String {
    #[cfg(target_os = "android")]
    if is_uri(path) {
        if let Some(name) = android::display_name(path) {
            return name;
        }
    }
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// Asks the user for a PDF with the system picker; returns its URI, or `None` if cancelled.
#[tauri::command]
pub async fn pick_pdf() -> Result<Option<String>, String> {
    #[cfg(target_os = "android")]
    return android::pick_pdf();
    #[cfg(not(target_os = "android"))]
    Err("solo disponible en Android".into())
}

/// Sends the app to the background (Android back button with nothing left to close).
#[tauri::command]
pub async fn leave_app() -> Result<(), String> {
    #[cfg(target_os = "android")]
    return android::move_to_back();
    #[cfg(not(target_os = "android"))]
    Err("solo disponible en Android".into())
}

/// Plugin that gives Rust access to the Android side; does nothing elsewhere.
pub fn plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("files")
        .setup(|_app, _api| {
            #[cfg(target_os = "android")]
            android::init(_app, _api)?;
            Ok(())
        })
        .build()
}

#[cfg(target_os = "android")]
mod android {
    use std::sync::OnceLock;

    use serde::Deserialize;
    use tauri::plugin::{PluginApi, PluginHandle};
    use tauri::{AppHandle, Wry};
    use tauri_plugin_fs::{FilePath, FsExt};

    struct Files {
        app: AppHandle<Wry>,
        plugin: PluginHandle<Wry>,
    }

    static FILES: OnceLock<Files> = OnceLock::new();

    pub fn init(app: &AppHandle<Wry>, api: PluginApi<Wry, ()>) -> Result<(), Box<dyn std::error::Error>> {
        use tauri::Manager;
        // Android has no writable /tmp; temp_dir() (undo history, saving) must point to the app cache.
        if let Ok(cache) = app.path().app_cache_dir() {
            let _ = std::fs::create_dir_all(&cache);
            // SAFETY: set once at startup, before any of our threads use temp_dir().
            unsafe { std::env::set_var("TMPDIR", cache) };
        }
        let app = app.clone();
        let plugin = api.register_android_plugin("com.romanarenas.pliego", "FilesPlugin")?;
        let _ = FILES.set(Files { app, plugin });
        Ok(())
    }

    fn files() -> Result<&'static Files, String> {
        FILES.get().ok_or_else(|| "el acceso a archivos no está listo".into())
    }

    fn uri(path: &str) -> FilePath {
        let Ok(uri) = path.parse();
        uri
    }

    pub fn read(path: &str) -> Result<Vec<u8>, String> {
        let files = files()?;
        files.app.fs().read(uri(path)).map_err(|e| format!("no se pudo leer el archivo: {e}"))
    }

    /// The bytes go through a temporary file because the plugin bridge only carries JSON.
    pub fn write(path: &str, bytes: &[u8]) -> Result<(), String> {
        let files = files()?;
        let source = std::env::temp_dir().join(format!("pliego-save-{}.tmp", std::process::id()));
        std::fs::write(&source, bytes).map_err(|e| format!("no se pudo guardar: {e}"))?;
        let args = serde_json::json!({ "uri": path, "source": source.to_string_lossy() });
        files
            .plugin
            .run_mobile_plugin::<serde_json::Value>("writeUri", args)
            .map(|_| ())
            .map_err(|_| "No se puede guardar en este archivo. Usa «Guardar como…» para elegir dónde.".to_string())
    }

    #[derive(Deserialize)]
    struct Picked {
        uri: Option<String>,
    }

    pub fn pick_pdf() -> Result<Option<String>, String> {
        let files = files()?;
        let picked = files
            .plugin
            .run_mobile_plugin::<Picked>("pickPdf", ())
            .map_err(|e| format!("no se pudo abrir el selector: {e}"))?;
        Ok(picked.uri)
    }

    pub fn move_to_back() -> Result<(), String> {
        files()?
            .plugin
            .run_mobile_plugin::<serde_json::Value>("moveToBack", ())
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    #[derive(Deserialize)]
    struct Name {
        name: Option<String>,
    }

    pub fn display_name(path: &str) -> Option<String> {
        let files = files().ok()?;
        files
            .plugin
            .run_mobile_plugin::<Name>("displayName", serde_json::json!({ "uri": path }))
            .ok()?
            .name
    }
}
