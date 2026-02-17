mod audio;
mod commands;
mod logging;
mod network;
mod queue;
mod session;
mod sync;
mod transfer;

use commands::AppState;

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
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::create_session,
            commands::join_session,
            commands::leave_session,
            commands::get_discovered_sessions,
            commands::play,
            commands::pause,
            commands::stop,
            commands::seek,
            commands::skip,
            commands::add_song,
            commands::remove_from_queue,
            commands::reorder_queue,
            commands::request_song,
            commands::accept_song_request,
            commands::reject_song_request,
            commands::set_volume,
        ])
        .run(tauri::generate_context!())
        .expect("error while running PhaseLock");
}
