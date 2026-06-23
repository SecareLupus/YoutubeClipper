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
        return name.to_string();
    }

    #[cfg(not(debug_assertions))]
    {
        let resource_dir = app.path().resource_dir().unwrap_or_default();
        let binaries_dir = resource_dir.join("binaries");
        let triple_name = format!("{}-{}", name, env!("TAURI_ENV_TARGET_TRIPLE"));

        eprintln!(
            "[better-clipper] resolve_sidecar({name}): resource_dir={resource_dir:?}, target={}",
            env!("TAURI_ENV_TARGET_TRIPLE")
        );

        let candidates: &[std::path::PathBuf] = &[
            resource_dir.join(name),
            binaries_dir.join(name),
            binaries_dir.join(&triple_name),
        ];

        for candidate in candidates {
            let exists = candidate.exists();
            eprintln!(
                "[better-clipper]   try {:?} -> {}",
                candidate,
                if exists { "FOUND" } else { "missing" }
            );
            if exists {
                return candidate.to_string_lossy().to_string();
            }
        }

        let fallback = binaries_dir.join(name);
        eprintln!("[better-clipper]   NOT FOUND — falling back to {fallback:?}");
        fallback.to_string_lossy().to_string()
    }
}
