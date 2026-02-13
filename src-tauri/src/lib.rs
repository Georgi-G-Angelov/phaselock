mod audio;
mod commands;
mod logging;
mod network;
mod queue;
mod session;
mod sync;
mod transfer;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![commands::greet])
        .run(tauri::generate_context!())
        .expect("error while running PhaseLock");
}
