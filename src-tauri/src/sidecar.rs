use tauri::AppHandle;

#[cfg(not(debug_assertions))]
use tauri::Manager;

/// Resolve a sidecar binary path.
///
/// In dev builds: returns the binary name directly (relies on PATH).
/// In production: resolves relative to the app's resource directory
/// where Tauri's `bundle.resources` places the binaries/ files.
/// Tries the plain name first, then falls back to `name-{target-triple}`
/// for compatibility with externalBin naming conventions.
pub fn resolve_sidecar(app: &AppHandle, name: &str) -> String {
    #[cfg(debug_assertions)]
    {
        let _ = app;
        name.to_string()
    }

    #[cfg(not(debug_assertions))]
    {
        let resource_dir = app.path().resource_dir().unwrap_or_default();

        // Try plain name first
        let plain = resource_dir.join(name);
        if plain.exists() {
            return plain.to_string_lossy().to_string();
        }

        // Fall back to name-target-triple (externalBin legacy)
        let triple = resource_dir.join(format!("{}-{}", name, env!("TAURI_ENV_TARGET_TRIPLE")));
        if triple.exists() {
            return triple.to_string_lossy().to_string();
        }

        // Return plain path anyway — caller will get a clearer "command not found" error
        plain.to_string_lossy().to_string()
    }
}
