use std::process::Stdio;
use tokio::process::Command;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Prefer formats Symphonia can decode: AAC (m4a), Vorbis, MP3.
/// Opus is NOT supported by Symphonia, so we exclude it.
const FORMAT: &str = "bestaudio[acodec=aac]/bestaudio[acodec=vorbis]/bestaudio[acodec=mp3]/bestaudio[ext=m4a]";

/// Information about a downloaded YouTube track.
pub struct YouTubeTrack {
    /// Raw audio bytes (WebM/Opus, M4A, etc.).
    pub audio_data: Vec<u8>,
    /// Video/track title from YouTube.
    pub title: String,
    /// Channel / artist name from YouTube.
    pub artist: String,
    /// Suggested file name (sanitised title + extension).
    pub file_name: String,
}

/// Lightweight metadata fetched before the full download.
#[derive(Debug, Clone)]
pub struct YouTubeMetadata {
    pub title: String,
    pub artist: String,
    pub file_name: String,
    /// The audio container extension (e.g. "m4a", "ogg").
    pub ext: String,
}

/// Build a safe filename from a video title.
/// Uses a blacklist of OS-forbidden characters so all Unicode scripts
/// (Cyrillic, CJK, Arabic, etc.) pass through untouched.
fn safe_file_name(title: &str, ext: &str) -> String {
    let safe: String = title
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == ':' || c == '*'
                || c == '?' || c == '"' || c == '<' || c == '>' || c == '|'
                || c.is_control()
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    format!("{}.{}", safe.trim(), ext)
}

/// Parse an "Artist - Title" pattern from a raw YouTube video title.
/// Returns (title, artist).  Falls back to (raw_title, uploader).
fn parse_artist_title(raw_title: &str, uploader: &str) -> (String, String) {
    if let Some(dash_pos) = raw_title.find('-') {
        let left = raw_title[..dash_pos].trim();
        let right = raw_title[dash_pos + 1..].trim();
        if !left.is_empty() && !right.is_empty() {
            return (right.to_string(), left.to_string());
        }
    }
    (raw_title.to_string(), uploader.to_string())
}

/// Quickly fetch only the metadata (title, artist, extension) for a YouTube URL.
///
/// This is fast (no audio download) and is used to populate the download queue UI.
pub async fn fetch_metadata(url: &str) -> Result<YouTubeMetadata, String> {
    let mut meta_cmd = Command::new("yt-dlp");
    meta_cmd
        .env("PYTHONIOENCODING", "utf-8")
        .args([
            "-f", FORMAT,
            "--no-playlist",
            "--print", "%(title)s\n%(uploader)s\n%(ext)s",
            "--no-warnings",
            url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    meta_cmd.creation_flags(CREATE_NO_WINDOW);

    let meta = meta_cmd.output()
        .await
        .map_err(|e| format!("Failed to run yt-dlp (is it installed?): {e}"))?;

    if !meta.status.success() {
        let stderr = String::from_utf8_lossy(&meta.stderr);
        return Err(format!("yt-dlp metadata failed: {stderr}"));
    }

    let meta_str = String::from_utf8_lossy(&meta.stdout);
    let lines: Vec<&str> = meta_str.trim().lines().collect();
    if lines.len() < 3 {
        return Err(format!("Unexpected yt-dlp metadata output: {meta_str}"));
    }

    let raw_title = lines[0].to_string();
    let uploader = lines[1].to_string();
    let ext = lines[2].to_string();

    let (title, artist) = parse_artist_title(&raw_title, &uploader);
    let file_name = safe_file_name(&format!("{} - {}", artist, title), &ext);

    log::info!("[youtube] Fetched metadata: \"{}\" by \"{}\" (raw: \"{}\")", title, artist, raw_title);

    Ok(YouTubeMetadata { title, artist, file_name, ext })
}

/// Download the audio bytes for a YouTube URL using `yt-dlp`.
///
/// Requires `yt-dlp` to be installed and on PATH.
pub async fn download_audio(url: &str) -> Result<YouTubeTrack, String> {
    // First fetch metadata.
    let meta = fetch_metadata(url).await?;

    log::info!(
        "[youtube] Downloading audio: \"{}\" by \"{}\" (format: {})",
        meta.title, meta.artist, meta.ext
    );

    // Download audio bytes to stdout.
    let mut dl_cmd = Command::new("yt-dlp");
    dl_cmd
        .env("PYTHONIOENCODING", "utf-8")
        .args([
            "-f", FORMAT,
            "--no-playlist",
            "-o", "-",          // output to stdout
            "--no-warnings",
            "--quiet",
            url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    dl_cmd.creation_flags(CREATE_NO_WINDOW);

    let dl = dl_cmd.output()
        .await
        .map_err(|e| format!("yt-dlp download failed: {e}"))?;

    if !dl.status.success() {
        let stderr = String::from_utf8_lossy(&dl.stderr);
        return Err(format!("yt-dlp download failed: {stderr}"));
    }

    let audio_data = dl.stdout;

    if audio_data.is_empty() {
        return Err("yt-dlp returned no audio data".into());
    }

    log::info!(
        "[youtube] Downloaded {} bytes for \"{}\" by \"{}\"",
        audio_data.len(), meta.title, meta.artist
    );

    Ok(YouTubeTrack {
        audio_data,
        title: meta.title,
        artist: meta.artist,
        file_name: meta.file_name,
    })
}

/// Search YouTube for a query and return the URL of the top result.
///
/// Uses yt-dlp's `ytsearch1:<query>` feature.
pub async fn search_youtube(query: &str) -> Result<String, String> {
    let search_term = format!("ytsearch1:{query}");

    let mut cmd = Command::new("yt-dlp");
    cmd
        .env("PYTHONIOENCODING", "utf-8")
        .args([
            &search_term,
            "--print", "%(webpage_url)s",
            "--no-download",
            "--no-playlist",
            "--no-warnings",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let output = cmd.output()
        .await
        .map_err(|e| format!("Failed to run yt-dlp search: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("yt-dlp search failed: {stderr}"));
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if url.is_empty() {
        return Err(format!("No results found for \"{query}\""));
    }

    log::info!("[youtube] Search \"{}\" → {}", query, url);
    Ok(url)
}
