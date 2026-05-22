use tauri::Manager;

mod downloader;
mod transcriber;

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! Schnitt is ready.", name)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_fs::init())
        .setup(|app| {
            // Ensure binaries and models directories exist
            let resource_dir = app.path().resource_dir()?;
            let binaries_dir = resource_dir.join("binaries");
            let models_dir = resource_dir.join("models");
            std::fs::create_dir_all(&binaries_dir)?;
            std::fs::create_dir_all(&models_dir)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![greet])
        .run(tauri::generate_context!())
        .expect("error while running Schnitt");
}
