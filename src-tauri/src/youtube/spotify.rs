//! Spotify playlist import (no credentials required).
//!
//! Fetches a public Spotify playlist page and extracts track info from the
//! JSON-LD structured data that Spotify embeds in the HTML for SEO.

use serde::Deserialize;

/// A single track extracted from a Spotify playlist.
#[derive(Debug, Clone)]
pub struct SpotifyTrack {
    pub name: String,
    pub artist: String,
}

/// Extract the playlist ID from a Spotify URL.
///
/// Accepts formats like:
/// - `https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M`
/// - `https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M?si=abc123`
fn extract_playlist_id(url: &str) -> Result<String, String> {
    let url = url.trim();

    // Strip query params.
    let path = url.split('?').next().unwrap_or(url);
    let path = path.split('#').next().unwrap_or(path);

    // Look for "/playlist/<id>" in the path.
    if let Some(idx) = path.find("/playlist/") {
        let rest = &path[idx + "/playlist/".len()..];
        let id = rest.split('/').next().unwrap_or(rest);
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }

    Err(format!("Could not extract playlist ID from URL: {url}"))
}

/// Returns `true` if the string looks like a Spotify playlist URL.
pub fn is_spotify_playlist_url(s: &str) -> bool {
    let s = s.trim();
    (s.contains("open.spotify.com/playlist/") || s.contains("spotify.com/playlist/"))
        && extract_playlist_id(s).is_ok()
}

/// Returns `true` if the string looks like a Spotify track URL.
pub fn is_spotify_track_url(s: &str) -> bool {
    let s = s.trim();
    (s.contains("open.spotify.com/track/") || s.contains("spotify.com/track/"))
        && extract_track_id(s).is_ok()
}

/// Returns `true` if the string looks like any Spotify URL we can handle.
pub fn is_spotify_url(s: &str) -> bool {
    is_spotify_playlist_url(s) || is_spotify_track_url(s)
}

/// Extract the track ID from a Spotify track URL.
fn extract_track_id(url: &str) -> Result<String, String> {
    let url = url.trim();
    let path = url.split('?').next().unwrap_or(url);
    let path = path.split('#').next().unwrap_or(path);

    if let Some(idx) = path.find("/track/") {
        let rest = &path[idx + "/track/".len()..];
        let id = rest.split('/').next().unwrap_or(rest);
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }

    Err(format!("Could not extract track ID from URL: {url}"))
}

// ── JSON-LD deserialization ─────────────────────────────────────────────────

/// Top-level JSON-LD (can be a playlist or a single recording).
#[derive(Debug, Deserialize)]
struct JsonLd {
    #[serde(rename = "@type")]
    ld_type: Option<String>,
    name: Option<String>,
    track: Option<Vec<JsonLdTrack>>,
    #[serde(rename = "byArtist")]
    by_artist: Option<JsonLdArtist>,
}

#[derive(Debug, Deserialize)]
struct JsonLdTrack {
    #[serde(rename = "@type")]
    _ld_type: Option<String>,
    name: Option<String>,
    #[serde(rename = "byArtist")]
    by_artist: Option<JsonLdArtist>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum JsonLdArtist {
    Single(JsonLdArtistObj),
    Multiple(Vec<JsonLdArtistObj>),
}

#[derive(Debug, Deserialize)]
struct JsonLdArtistObj {
    name: Option<String>,
}

/// Fetch all tracks from a public Spotify playlist.
///
/// Strategy: GET the playlist page HTML, extract the `<script type="application/ld+json">`
/// block, and parse track names + artists from the structured data.
pub async fn fetch_playlist_tracks(url: &str) -> Result<Vec<SpotifyTrack>, String> {
    let playlist_id = extract_playlist_id(url)?;
    let page_url = format!("https://open.spotify.com/playlist/{playlist_id}");

    log::info!("[spotify] Fetching playlist page: {page_url}");

    let client = build_client()?;

    let resp = client
        .get(&page_url)
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch Spotify page: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("Spotify returned HTTP {}", resp.status()));
    }

    let html = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read Spotify response: {e}"))?;

    log::info!("[spotify] Got {} bytes of HTML", html.len());

    // Extract JSON-LD blocks from the HTML.
    let tracks = parse_json_ld_tracks(&html)?;

    if tracks.is_empty() {
        return Err("No tracks found in playlist. The playlist may be empty or private.".into());
    }

    log::info!("[spotify] Extracted {} tracks from playlist", tracks.len());
    Ok(tracks)
}

/// Fetch a single track's info from a public Spotify track page.
///
/// Uses the same JSON-LD strategy — the track page embeds a MusicRecording.
pub async fn fetch_track_info(url: &str) -> Result<SpotifyTrack, String> {
    let track_id = extract_track_id(url)?;
    let page_url = format!("https://open.spotify.com/track/{track_id}");

    log::info!("[spotify] Fetching track page: {page_url}");

    let client = build_client()?;

    let resp = client
        .get(&page_url)
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch Spotify page: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("Spotify returned HTTP {}", resp.status()));
    }

    let html = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read Spotify response: {e}"))?;

    log::info!("[spotify] Got {} bytes of HTML", html.len());

    parse_json_ld_single_track(&html)
}

/// Build a shared reqwest client with a realistic User-Agent.
fn build_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))
}

/// Parse `<script type="application/ld+json">` blocks from HTML and extract tracks.
fn parse_json_ld_tracks(html: &str) -> Result<Vec<SpotifyTrack>, String> {
    let tag_open = r#"<script type="application/ld+json">"#;
    let tag_close = "</script>";

    let mut tracks = Vec::new();

    let mut search_from = 0;
    while let Some(start) = html[search_from..].find(tag_open) {
        let json_start = search_from + start + tag_open.len();
        if let Some(end) = html[json_start..].find(tag_close) {
            let json_str = &html[json_start..json_start + end];

            // Try to parse as our JSON-LD structure.
            if let Ok(ld) = serde_json::from_str::<JsonLd>(json_str) {
                if ld.ld_type.as_deref() == Some("MusicPlaylist") {
                    if let Some(ld_tracks) = ld.track {
                        for t in ld_tracks {
                            let name = t.name.unwrap_or_default();
                            if name.is_empty() {
                                continue;
                            }
                            let artist = extract_artist_name(t.by_artist);
                            tracks.push(SpotifyTrack { name, artist });
                        }
                    }
                }
            }

            search_from = json_start + end;
        } else {
            break;
        }
    }

    if tracks.is_empty() {
        return Err(
            "Could not find track data in the Spotify page. \
             The playlist may be private, empty, or the page format has changed."
                .into(),
        );
    }

    Ok(tracks)
}

/// Parse a single MusicRecording from JSON-LD (for track pages).
fn parse_json_ld_single_track(html: &str) -> Result<SpotifyTrack, String> {
    let tag_open = r#"<script type="application/ld+json">"#;
    let tag_close = "</script>";

    let mut search_from = 0;
    while let Some(start) = html[search_from..].find(tag_open) {
        let json_start = search_from + start + tag_open.len();
        if let Some(end) = html[json_start..].find(tag_close) {
            let json_str = &html[json_start..json_start + end];

            if let Ok(ld) = serde_json::from_str::<JsonLd>(json_str) {
                if ld.ld_type.as_deref() == Some("MusicRecording") {
                    let name = ld.name.unwrap_or_default();
                    if !name.is_empty() {
                        let artist = extract_artist_name(ld.by_artist);
                        return Ok(SpotifyTrack { name, artist });
                    }
                }
            }

            search_from = json_start + end;
        } else {
            break;
        }
    }

    Err("Could not find track data in the Spotify page.".into())
}

/// Extract a human-readable artist string from a JsonLdArtist enum.
fn extract_artist_name(by_artist: Option<JsonLdArtist>) -> String {
    match by_artist {
        Some(JsonLdArtist::Single(a)) => a.name.unwrap_or_else(|| "Unknown".into()),
        Some(JsonLdArtist::Multiple(artists)) => {
            let names: Vec<String> = artists.into_iter().filter_map(|a| a.name).collect();
            if names.is_empty() { "Unknown".into() } else { names.join(", ") }
        }
        None => "Unknown".into(),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_playlist_id() {
        assert_eq!(
            extract_playlist_id("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M").unwrap(),
            "37i9dQZF1DXcBWIGoYBM5M"
        );
        assert_eq!(
            extract_playlist_id("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M?si=abc123").unwrap(),
            "37i9dQZF1DXcBWIGoYBM5M"
        );
        assert!(extract_playlist_id("https://open.spotify.com/track/123").is_err());
    }

    #[test]
    fn test_is_spotify_url() {
        assert!(is_spotify_playlist_url("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M"));
        assert!(is_spotify_playlist_url("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M?si=x"));
        assert!(!is_spotify_playlist_url("https://www.youtube.com/watch?v=abc"));
        assert!(!is_spotify_playlist_url("some random query"));

        assert!(is_spotify_track_url("https://open.spotify.com/track/4PTG3Z6ehGkBFwjybzWkR8"));
        assert!(is_spotify_track_url("https://open.spotify.com/track/4PTG3Z6ehGkBFwjybzWkR8?si=abc"));
        assert!(!is_spotify_track_url("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M"));

        assert!(is_spotify_url("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M"));
        assert!(is_spotify_url("https://open.spotify.com/track/4PTG3Z6ehGkBFwjybzWkR8"));
        assert!(!is_spotify_url("random text"));
    }

    #[test]
    fn test_parse_json_ld() {
        let html = r#"
            <html><head>
            <script type="application/ld+json">
            {
                "@type": "MusicPlaylist",
                "name": "Test Playlist",
                "track": [
                    {
                        "@type": "MusicRecording",
                        "name": "Song One",
                        "byArtist": {"name": "Artist A"}
                    },
                    {
                        "@type": "MusicRecording",
                        "name": "Song Two",
                        "byArtist": {"name": "Artist B"}
                    }
                ]
            }
            </script>
            </head></html>
        "#;

        let tracks = parse_json_ld_tracks(html).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].name, "Song One");
        assert_eq!(tracks[0].artist, "Artist A");
        assert_eq!(tracks[1].name, "Song Two");
        assert_eq!(tracks[1].artist, "Artist B");
    }

    #[test]
    fn test_parse_json_ld_single_track() {
        let html = r#"
            <html><head>
            <script type="application/ld+json">
            {
                "@type": "MusicRecording",
                "name": "Bohemian Rhapsody",
                "byArtist": {"name": "Queen"}
            }
            </script>
            </head></html>
        "#;

        let track = parse_json_ld_single_track(html).unwrap();
        assert_eq!(track.name, "Bohemian Rhapsody");
        assert_eq!(track.artist, "Queen");
    }
}
