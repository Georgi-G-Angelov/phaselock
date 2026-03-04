pub mod download;
pub mod queue;
pub mod spotify;

use std::process::Stdio;
use std::sync::RwLock;
use tauri::{AppHandle, Emitter};
use tokio::process::Command;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Payload emitted via `ytdlp:status` to the frontend.
#[derive(Clone, serde::Serialize)]
pub struct YtDlpStatus {
    /// One of: "checking", "updating", "ready", "unavailable"
    pub state: String,
    /// Human-readable detail to show in the banner.
    pub message: String,
}

/// Latest yt-dlp status, persisted so late-mounting components can query it.
static YTDLP_STATE: RwLock<Option<YtDlpStatus>> = RwLock::new(None);

fn emit_status(app: &AppHandle, state: &str, message: &str) {
    let status = YtDlpStatus {
        state: state.into(),
        message: message.into(),
    };
    let _ = app.emit("ytdlp:status", status.clone());
    // Persist so components mounting later can read the current state.
    if let Ok(mut guard) = YTDLP_STATE.write() {
        *guard = Some(status);
    }
}

/// Return the current yt-dlp readiness status (non-async, no lock contention).
pub fn current_status() -> YtDlpStatus {
    YTDLP_STATE
        .read()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or(YtDlpStatus {
            state: "checking".into(),
            message: "Checking yt-dlp…".into(),
        })
}

/// Try to install / update yt-dlp via `pip` if needed.
///
/// 1. Runs `yt-dlp --version` to get the installed version (instant, no network).
/// 2. Checks PyPI for the latest published version (one small HTTP request).
/// 3. Only runs `pip install -U yt-dlp` if the versions differ or yt-dlp is missing.
///
/// Emits `ytdlp:status` events so the frontend can show a banner.
pub async fn update_ytdlp(app: AppHandle) {
    log::info!("[yt-dlp] Checking yt-dlp version…");
    emit_status(&app, "checking", "Checking yt-dlp…");

    let installed = get_installed_version().await;
    let latest = get_latest_pypi_version().await;

    match (&installed, &latest) {
        (Some(cur), Some(lat)) if cur == lat => {
            log::info!("[yt-dlp] Already up to date ({cur})");
            emit_status(&app, "ready", "");
            return;
        }
        (Some(cur), Some(lat)) => {
            log::info!("[yt-dlp] Update available: {cur} → {lat}");
            emit_status(&app, "updating", &format!("Updating yt-dlp ({cur} → {lat})…"));
        }
        (None, _) => {
            log::info!("[yt-dlp] Not installed, will install via pip");
            emit_status(&app, "updating", "Installing yt-dlp…");
        }
        (Some(cur), None) => {
            log::info!("[yt-dlp] Installed ({cur}), could not check latest — skipping update");
            emit_status(&app, "ready", "");
            return;
        }
    }

    run_pip_install().await;

    // Verify installation succeeded.
    if get_installed_version().await.is_some() {
        emit_status(&app, "ready", "");
    } else {
        emit_status(&app, "unavailable", "yt-dlp could not be installed. YouTube/Spotify features unavailable.");
    }
}

/// Get the currently installed yt-dlp version, or `None` if not installed.
async fn get_installed_version() -> Option<String> {
    let mut cmd = Command::new("yt-dlp");
    cmd.arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);

    match cmd.output().await {
        Ok(output) if output.status.success() => {
            let ver = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if ver.is_empty() { None } else { Some(ver) }
        }
        _ => None,
    }
}

/// Fetch the latest yt-dlp version string from PyPI.
async fn get_latest_pypi_version() -> Option<String> {
    #[derive(serde::Deserialize)]
    struct PyPiResponse {
        info: PyPiInfo,
    }
    #[derive(serde::Deserialize)]
    struct PyPiInfo {
        version: String,
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;

    let resp = client
        .get("https://pypi.org/pypi/yt-dlp/json")
        .send()
        .await
        .ok()?;

    let data: PyPiResponse = resp.json().await.ok()?;
    Some(data.info.version)
}

/// Run `pip install -U yt-dlp`.
async fn run_pip_install() {
    for python in &["python", "python3"] {
        let mut cmd = Command::new(python);
        cmd.args(["-m", "pip", "install", "-U", "yt-dlp", "--quiet"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(target_os = "windows")]
        cmd.creation_flags(CREATE_NO_WINDOW);

        match cmd.output().await {
            Ok(output) if output.status.success() => {
                log::info!("[yt-dlp] yt-dlp updated successfully (via {python})");
                return;
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                log::debug!("[yt-dlp] `{python} -m pip install -U yt-dlp` failed: {stderr}");
            }
            Err(e) => {
                log::debug!("[yt-dlp] Could not run `{python}`: {e}");
            }
        }
    }

    log::warn!(
        "[yt-dlp] Could not update yt-dlp via pip. \
         Make sure Python 3 and pip are installed."
    );
}