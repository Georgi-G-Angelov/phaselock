// Many structs / methods are scaffolded for upcoming networked-playback
// features but not yet wired into commands.  Silence the noise until they
// are connected.
#![allow(dead_code)]

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
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                use tauri::Manager;
                let app = window.app_handle().clone();
                tauri::async_runtime::spawn(async move {
                    let state = app.state::<AppState>();
                    let mut session = state.session.lock().await;
                    if session.is_active() {
                        log::info!("Window closing — shutting down active session");
                        session.shutdown().await;
                    }
                    // Stop mDNS browser if running.
                    let mut browser = state.mdns_browser.lock().await;
                    if let Some(b) = browser.take() {
                        let _ = b.stop();
                    }
                    // Stop position ticker.
                    commands::stop_position_ticker(&state).await;
                    log::info!("Graceful shutdown complete");
                });
            }
        })
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
