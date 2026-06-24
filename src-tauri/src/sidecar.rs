use tauri::AppHandle;

#[cfg(not(debug_assertions))]
use tauri::Manager;

/// Resolve a sidecar binary path.
///
/// In dev builds: returns the binary name directly (relies on PATH).
/// In production: checks the host's PATH first, then falls back to
/// bundled resources. This lets the user's system-provided yt-dlp
/// and ffmpeg take priority over bundled versions.
pub fn resolve_sidecar(app: &AppHandle, name: &str) -> String {
    #[cfg(debug_assertions)]
    {
        let _ = app;
        return name.to_string();
    }

    #[cfg(not(debug_assertions))]
    {
        // Check host PATH first — user's system version is always preferred
        if let Ok(path) = which::which(name) {
            let s = path.to_string_lossy().to_string();
            eprintln!("[better-clipper] resolve_sidecar({name}): found on PATH -> {s}");
            return s;
        }

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
