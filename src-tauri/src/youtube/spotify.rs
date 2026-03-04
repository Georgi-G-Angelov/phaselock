//! Spotify playlist/track import via embed page scraping.
//!
//! The Spotify embed pages at `open.spotify.com/embed/playlist/...` (and
//! `.../embed/track/...`) are server-rendered and contain a
//! `<script id="__NEXT_DATA__">` tag with JSON that includes the full
//! track list (title + artist).  No API tokens or credentials needed.

use serde::Deserialize;

/// A single track extracted from a Spotify playlist or track URL.
#[derive(Debug, Clone)]
pub struct SpotifyTrack {
    pub name: String,
    pub artist: String,
}

// ── URL helpers ─────────────────────────────────────────────────────────────

/// Extract the playlist ID from a Spotify URL.
fn extract_playlist_id(url: &str) -> Result<String, String> {
    let url = url.trim();
    let path = url.split('?').next().unwrap_or(url);
    let path = path.split('#').next().unwrap_or(path);

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

// ── Embed page JSON types ───────────────────────────────────────────────────

/// Track entry inside the `entity.trackList` array of __NEXT_DATA__.
#[derive(Debug, Deserialize)]
struct EmbedTrack {
    title: Option<String>,
    subtitle: Option<String>,
}

// ── HTTP + parsing helpers ──────────────────────────────────────────────────

/// Build a reqwest client with a realistic browser User-Agent.
fn build_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
             AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/120.0.0.0 Safari/537.36",
        )
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))
}

/// Fetch the Spotify embed page and extract the `__NEXT_DATA__` JSON.
async fn fetch_embed_data(
    client: &reqwest::Client,
    kind: &str,       // "playlist" or "track"
    spotify_id: &str,
) -> Result<serde_json::Value, String> {
    let embed_url = format!("https://open.spotify.com/embed/{kind}/{spotify_id}");
    log::info!("[spotify] Fetching embed page: {embed_url}");

    let resp = client
        .get(&embed_url)
        .send()
        .await
        .map_err(|e| format!("Failed to fetch Spotify embed page: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!(
            "Spotify embed page returned HTTP {}",
            resp.status()
        ));
    }

    let html = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read embed page: {e}"))?;

    log::info!("[spotify] Embed page: {} bytes", html.len());

    // Extract <script id="__NEXT_DATA__" type="application/json">{...}</script>
    let marker = r#"<script id="__NEXT_DATA__" type="application/json">"#;
    let start = html
        .find(marker)
        .ok_or("Could not find __NEXT_DATA__ in embed page")?;
    let json_start = start + marker.len();
    let json_end = html[json_start..]
        .find("</script>")
        .ok_or("Malformed __NEXT_DATA__ script tag")?;
    let json_str = &html[json_start..json_start + json_end];

    serde_json::from_str(json_str)
        .map_err(|e| format!("Failed to parse __NEXT_DATA__ JSON: {e}"))
}

/// Extract tracks from the __NEXT_DATA__ JSON.
///
/// The tracks live at `props.pageProps.state.data.entity.trackList`.
fn extract_tracks_from_data(data: &serde_json::Value) -> Result<Vec<SpotifyTrack>, String> {
    let track_list = data
        .pointer("/props/pageProps/state/data/entity/trackList")
        .and_then(|v| v.as_array())
        .ok_or(
            "Could not find trackList in embed page data. \
             The page structure may have changed.",
        )?;

    let mut tracks = Vec::new();
    for entry in track_list {
        if let Ok(t) = serde_json::from_value::<EmbedTrack>(entry.clone()) {
            let name = t.title.unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let artist = t.subtitle.unwrap_or_else(|| "Unknown".into());
            tracks.push(SpotifyTrack { name, artist });
        }
    }

    Ok(tracks)
}

/// Extract a single track's info from the __NEXT_DATA__ JSON (track embed).
///
/// For single tracks the info lives at `props.pageProps.state.data.entity`
/// with `title` and `subtitle` fields.
fn extract_single_track_from_data(data: &serde_json::Value) -> Result<SpotifyTrack, String> {
    let entity = data
        .pointer("/props/pageProps/state/data/entity")
        .ok_or("Could not find entity in embed page data.")?;

    let name = entity
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    if name.is_empty() {
        return Err("Track title is empty in embed data.".into());
    }

    let artist = entity
        .get("subtitle")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown")
        .to_string();

    Ok(SpotifyTrack { name, artist })
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Fetch all tracks from a public Spotify playlist.
pub async fn fetch_playlist_tracks(url: &str) -> Result<Vec<SpotifyTrack>, String> {
    let playlist_id = extract_playlist_id(url)?;
    let client = build_client()?;
    let data = fetch_embed_data(&client, "playlist", &playlist_id).await?;

    let tracks = extract_tracks_from_data(&data)?;

    if tracks.is_empty() {
        return Err("No tracks found. The playlist may be empty or private.".into());
    }

    log::info!("[spotify] Extracted {} tracks from playlist", tracks.len());
    Ok(tracks)
}

/// Fetch a single track's info from a Spotify track URL.
pub async fn fetch_track_info(url: &str) -> Result<SpotifyTrack, String> {
    let track_id = extract_track_id(url)?;
    let client = build_client()?;
    let data = fetch_embed_data(&client, "track", &track_id).await?;

    let track = extract_single_track_from_data(&data)?;
    log::info!("[spotify] Track: \"{}\" by \"{}\"", track.name, track.artist);
    Ok(track)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_playlist_id() {
        assert_eq!(
            extract_playlist_id("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M")
                .unwrap(),
            "37i9dQZF1DXcBWIGoYBM5M"
        );
        assert_eq!(
            extract_playlist_id(
                "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M?si=abc123"
            )
            .unwrap(),
            "37i9dQZF1DXcBWIGoYBM5M"
        );
        assert!(extract_playlist_id("https://open.spotify.com/track/123").is_err());
    }

    #[test]
    fn test_is_spotify_url() {
        assert!(is_spotify_playlist_url(
            "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M"
        ));
        assert!(is_spotify_playlist_url(
            "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M?si=x"
        ));
        assert!(!is_spotify_playlist_url("https://www.youtube.com/watch?v=abc"));
        assert!(!is_spotify_playlist_url("some random query"));

        assert!(is_spotify_track_url(
            "https://open.spotify.com/track/4PTG3Z6ehGkBFwjybzWkR8"
        ));
        assert!(is_spotify_track_url(
            "https://open.spotify.com/track/4PTG3Z6ehGkBFwjybzWkR8?si=abc"
        ));
        assert!(!is_spotify_track_url(
            "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M"
        ));

        assert!(is_spotify_url(
            "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M"
        ));
        assert!(is_spotify_url(
            "https://open.spotify.com/track/4PTG3Z6ehGkBFwjybzWkR8"
        ));
        assert!(!is_spotify_url("random text"));
    }

    #[test]
    fn test_extract_tracks_from_data() {
        let json = serde_json::json!({
            "props": {
                "pageProps": {
                    "state": {
                        "data": {
                            "entity": {
                                "trackList": [
                                    { "title": "Song One", "subtitle": "Artist A" },
                                    { "title": "Song Two", "subtitle": "Artist B" },
                                    { "title": "", "subtitle": "Skip Me" }
                                ]
                            }
                        }
                    }
                }
            }
        });

        let tracks = extract_tracks_from_data(&json).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].name, "Song One");
        assert_eq!(tracks[0].artist, "Artist A");
        assert_eq!(tracks[1].name, "Song Two");
        assert_eq!(tracks[1].artist, "Artist B");
    }

    #[test]
    fn test_extract_single_track_from_data() {
        let json = serde_json::json!({
            "props": {
                "pageProps": {
                    "state": {
                        "data": {
                            "entity": {
                                "title": "Bohemian Rhapsody",
                                "subtitle": "Queen"
                            }
                        }
                    }
                }
            }
        });

        let track = extract_single_track_from_data(&json).unwrap();
        assert_eq!(track.name, "Bohemian Rhapsody");
        assert_eq!(track.artist, "Queen");
    }
}
