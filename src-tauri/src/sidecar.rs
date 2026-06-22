use tauri::AppHandle;

#[cfg(not(debug_assertions))]
use tauri::Manager;

/// Resolve a sidecar binary path.
///
/// In dev builds: returns the binary name directly (relies on PATH).
/// In production: resolves relative to the app's resource directory
/// where Tauri's `bundle.resources` places the binaries/ files.
pub fn resolve_sidecar(app: &AppHandle, name: &str) -> String {
    #[cfg(debug_assertions)]
    {
        let _ = app;
        name.to_string()
    }

    #[cfg(not(debug_assertions))]
    {
        // Sidecars are bundled as resources, not externalBin — they live
        // in the resource directory (e.g. /usr/lib/better-clipper/ for deb,
        // or inside the AppDir for AppImage). No conflicts with system packages.
        app.path()
            .resource_dir()
            .unwrap_or_default()
            .join(name)
            .to_string_lossy()
            .to_string()
    }
}
