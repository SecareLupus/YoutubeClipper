use tauri::AppHandle;

#[cfg(not(debug_assertions))]
use tauri::Manager;

/// Resolve a sidecar binary path.
///
/// In dev builds: returns the binary name directly (relies on PATH).
/// In production: resolves relative to the app's resource directory.
/// Tries (in order):
///   1. `resource_dir/name`
///   2. `resource_dir/binaries/name`
///   3. `resource_dir/binaries/name-{target-triple}` (externalBin legacy)
pub fn resolve_sidecar(app: &AppHandle, name: &str) -> String {
    #[cfg(debug_assertions)]
    {
        let _ = app;
        name.to_string()
    }

    #[cfg(not(debug_assertions))]
    {
        let resource_dir = app.path().resource_dir().unwrap_or_default();
        let binaries_dir = resource_dir.join("binaries");
        let triple_name = format!("{}-{}", name, env!("TAURI_ENV_TARGET_TRIPLE"));

        // Check each possible location
        let candidates: &[std::path::PathBuf] = &[
            resource_dir.join(name),               // resource root, plain name
            binaries_dir.join(name),               // binaries/ subdir, plain name
            binaries_dir.join(&triple_name),       // binaries/ subdir, triple-suffixed
        ];

        for candidate in candidates {
            if candidate.exists() {
                return candidate.to_string_lossy().to_string();
            }
        }

        // Fall back to the most likely path so the OS gives a clear error
        binaries_dir.join(name).to_string_lossy().to_string()
    }
}
