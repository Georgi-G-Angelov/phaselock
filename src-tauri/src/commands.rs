/// Placeholder command for verifying IPC works.
#[tauri::command]
pub fn greet(name: &str) -> String {
    format!("Hello, {}! Welcome to PhaseLock.", name)
}
