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
    // Initialize the logger before anything else.
    let log_dir = dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("phaselock");
    if let Err(e) = logging::logger::init_logger(log_dir) {
        eprintln!("Failed to initialize logger: {e}");
    }

    log::info!("PhaseLock starting up");

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![commands::greet])
        .run(tauri::generate_context!())
        .expect("error while running PhaseLock");
}
