use tauri::AppHandle;

#[cfg(not(debug_assertions))]
use tauri::Manager;

/// Resolve a sidecar binary path.
///
/// In dev builds: returns the binary name directly (relies on PATH).
/// In production: prefers the bundled (pinned) resource, then falls back to
/// the host's PATH. Bundled binaries are deterministic, so a stale system
/// yt-dlp can't silently override a freshly-bundled one.
pub fn resolve_sidecar(app: &AppHandle, name: &str) -> String {
    #[cfg(debug_assertions)]
    {
        let _ = app;
        return name.to_string();
    }

    #[cfg(not(debug_assertions))]
    {
        // Prefer the bundled (pinned, known-good) binary so the app's behavior
        // is deterministic regardless of what's on the host PATH. Fall back to
        // the host PATH so users without a bundled copy still work.
        let resource_dir = app.path().resource_dir().unwrap_or_default();
        let binaries_dir = resource_dir.join("binaries");
        let triple_name = format!("{}-{}", name, env!("TAURI_ENV_TARGET_TRIPLE"));

        eprintln!(
            "[better-clipper] resolve_sidecar({name}): resource_dir={resource_dir:?}, target={}",
            env!("TAURI_ENV_TARGET_TRIPLE")
        );

        let exe = std::env::consts::EXE_SUFFIX; // ".exe" on Windows, "" elsewhere
        let candidates: &[std::path::PathBuf] = &[
            resource_dir.join(format!("{name}{exe}")),
            binaries_dir.join(format!("{name}{exe}")),
            binaries_dir.join(format!("{triple_name}{exe}")),
            binaries_dir.join(name),
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

        // Fall back to host PATH if no bundled binary is present.
        if let Ok(path) = which::which(name) {
            let s = path.to_string_lossy().to_string();
            eprintln!("[better-clipper] resolve_sidecar({name}): not bundled, using PATH -> {s}");
            return s;
        }

        let fallback = binaries_dir.join(name);
        eprintln!("[better-clipper]   NOT FOUND — falling back to {fallback:?}");
        fallback.to_string_lossy().to_string()
    }
}
