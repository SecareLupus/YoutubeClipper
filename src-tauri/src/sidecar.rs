/// Resolve a sidecar binary path.
///
/// In dev builds: returns the binary name directly (relies on PATH).
/// In production: returns the Tauri-sidecar path convention
/// `binaries/<name>-<target-triple>` for bundling.
pub fn resolve_sidecar(name: &str) -> String {
    #[cfg(debug_assertions)]
    {
        name.to_string()
    }

    #[cfg(not(debug_assertions))]
    {
        let target_triple = env!("TAURI_ENV_TARGET_TRIPLE");
        format!("binaries/{}-{}", name, target_triple)
    }
}
