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
        .setup(|app| {
            commands::setup_auto_advance_listener(app.handle());
            Ok(())
        })
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
                    // Stop audio playback and position ticker.
                    commands::stop_position_ticker(&state).await;
                    {
                        let audio = state.audio_output.lock().await;
                        if let Some(ref ao) = *audio {
                            ao.stop();
                        }
                    }
                    log::info!("Graceful shutdown complete");
                });
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::session::create_session,
            commands::session::join_session,
            commands::session::leave_session,
            commands::session::get_discovered_sessions,
            commands::playback::play,
            commands::playback::pause,
            commands::playback::stop,
            commands::playback::seek,
            commands::playback::skip,
            commands::playback::back,
            commands::queue::add_song,
            commands::queue::remove_from_queue,
            commands::queue::reorder_queue,
            commands::requests::request_song,
            commands::requests::accept_song_request,
            commands::requests::reject_song_request,
            commands::playback::set_volume,
        ])
        .run(tauri::generate_context!())
        .expect("error while running PhaseLock");
}
