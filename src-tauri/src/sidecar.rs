use tauri::AppHandle;

#[cfg(not(debug_assertions))]
use tauri::Manager;

/// Resolve a sidecar binary path.
///
/// In dev builds: returns the binary name directly (relies on PATH).
/// In production: resolves relative to the executable's directory
/// where Tauri's bundler places `externalBin` files alongside the app binary.
pub fn resolve_sidecar(app: &AppHandle, name: &str) -> String {
    #[cfg(debug_assertions)]
    {
        let _ = app;
        name.to_string()
    }

    #[cfg(not(debug_assertions))]
    {
        // In packaged builds, externalBin sidecars live next to the executable.
        // Fall back to resource_dir if the exe path can't be determined.
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| app.path().resource_dir().unwrap_or_default());

        exe_dir.join(name).to_string_lossy().to_string()
    }
}
