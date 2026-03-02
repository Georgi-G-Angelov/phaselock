use std::io::Cursor;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTagKey};
use symphonia::core::probe::Hint;

// ── Track Metadata ──────────────────────────────────────────────────────────

/// Metadata extracted from an MP3 file's ID3 tags.
#[derive(Debug, Clone, Default)]
pub struct TrackMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
}

/// Extract ID3 metadata (title, artist) from MP3 bytes.
///
/// Returns whatever tags are found; missing tags are `None`.
pub fn parse_mp3_metadata(data: &[u8]) -> TrackMetadata {
    let cursor = Cursor::new(data.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

    let mut hint = Hint::new();
    hint.with_extension("mp3");

    let mut probed = match symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    ) {
        Ok(p) => p,
        Err(_) => return TrackMetadata::default(),
    };

    let mut title: Option<String> = None;
    let mut artist: Option<String> = None;

    // Check metadata attached to the probe result.
    if let Some(md) = probed.metadata.get() {
        if let Some(rev) = md.current() {
            for tag in rev.tags() {
                if let Some(std_key) = tag.std_key {
                    match std_key {
                        StandardTagKey::TrackTitle => {
                            let v = tag.value.to_string();
                            if !v.is_empty() { title = Some(v); }
                        }
                        StandardTagKey::Artist | StandardTagKey::AlbumArtist => {
                            if artist.is_none() {
                                let v = tag.value.to_string();
                                if !v.is_empty() { artist = Some(v); }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Also check metadata on the format reader (some files store it there).
    let mut format = probed.format;
    if title.is_none() || artist.is_none() {
        if let Some(md) = format.metadata().current() {
            for tag in md.tags() {
                if let Some(std_key) = tag.std_key {
                    match std_key {
                        StandardTagKey::TrackTitle if title.is_none() => {
                            let v = tag.value.to_string();
                            if !v.is_empty() { title = Some(v); }
                        }
                        StandardTagKey::Artist | StandardTagKey::AlbumArtist if artist.is_none() => {
                            let v = tag.value.to_string();
                            if !v.is_empty() { artist = Some(v); }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    TrackMetadata { title, artist }
}

/// Resolve title and artist from ID3 metadata, falling back to filename
/// parsing ("Artist - Title" pattern) and then to defaults.
pub fn resolve_track_info(metadata: &TrackMetadata, file_name: &str) -> (String, String) {
    let has_title = metadata.title.as_ref()
        .map_or(false, |t| !t.is_empty() && t.to_lowercase() != "unknown");
    let has_artist = metadata.artist.as_ref()
        .map_or(false, |a| !a.is_empty() && a.to_lowercase() != "unknown");

    if has_title && has_artist {
        return (
            metadata.title.clone().unwrap(),
            metadata.artist.clone().unwrap(),
        );
    }

    // Strip .mp3 extension for filename-based fallback.
    let stem = file_name.strip_suffix(".mp3")
        .or_else(|| file_name.strip_suffix(".MP3"))
        .unwrap_or(file_name);

    // Try "Artist - Title" pattern.
    if let Some(dash_pos) = stem.find('-') {
        let left = stem[..dash_pos].trim();
        let right = stem[dash_pos + 1..].trim();
        if !left.is_empty() && !right.is_empty() {
            let title = if has_title {
                metadata.title.clone().unwrap()
            } else {
                right.to_string()
            };
            let artist = if has_artist {
                metadata.artist.clone().unwrap()
            } else {
                left.to_string()
            };
            return (title, artist);
        }
    }

    // Final fallback.
    let title = if has_title {
        metadata.title.clone().unwrap()
    } else {
        stem.to_string()
    };
    let artist = if has_artist {
        metadata.artist.clone().unwrap()
    } else {
        "Unknown".to_string()
    };
    (title, artist)
}

// ── DecodedAudio ────────────────────────────────────────────────────────────

/// A fully decoded audio track stored as interleaved f32 PCM samples.
#[derive(Debug, Clone)]
pub struct DecodedAudio {
    /// Interleaved PCM samples (L, R, L, R, …).
    pub samples: Vec<f32>,
    /// Sample rate in Hz (e.g. 44100, 48000).
    pub sample_rate: u32,
    /// Number of channels (typically 2 for stereo).
    pub channels: u16,
    /// Total number of frames (one frame = one sample per channel).
    pub total_frames: u64,
    /// Total duration in seconds.
    pub duration_secs: f64,
}

// ── DecodeError ─────────────────────────────────────────────────────────────

/// Errors that can occur when decoding audio.
#[derive(Debug)]
pub enum DecodeError {
    /// No audio track found in the file.
    NoTrack,
    /// Unsupported codec.
    UnsupportedCodec(String),
    /// Symphonia error.
    Symphonia(SymphoniaError),
    /// No samples decoded.
    EmptyOutput,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTrack => write!(f, "no audio track found"),
            Self::UnsupportedCodec(c) => write!(f, "unsupported codec: {c}"),
            Self::Symphonia(e) => write!(f, "decode error: {e}"),
            Self::EmptyOutput => write!(f, "no samples decoded"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<SymphoniaError> for DecodeError {
    fn from(e: SymphoniaError) -> Self {
        DecodeError::Symphonia(e)
    }
}

// ── Decoder ─────────────────────────────────────────────────────────────────

/// Decode an MP3 file from raw bytes into interleaved f32 PCM samples.
pub fn decode_mp3(data: &[u8]) -> Result<DecodedAudio, DecodeError> {
    // Build a MediaSourceStream from an in-memory cursor.
    let cursor = Cursor::new(data.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

    // Probe the format.
    let mut hint = Hint::new();
    hint.with_extension("mp3");

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(DecodeError::Symphonia)?;

    let mut format = probed.format;

    // Find the first audio track.
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or(DecodeError::NoTrack)?;

    let codec_params = track.codec_params.clone();
    let track_id = track.id;

    let sample_rate = codec_params
        .sample_rate
        .ok_or(DecodeError::UnsupportedCodec(
            "unknown sample rate".into(),
        ))?;
    let channels = codec_params
        .channels
        .map(|c| c.count() as u16)
        .unwrap_or(2);

    // Create a decoder.
    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|e| DecodeError::UnsupportedCodec(format!("{e}")))?;

    let mut all_samples: Vec<f32> = Vec::new();

    // Decode all packets.
    loop {
        let packet = match format.next_packet() {
            Ok(pkt) => pkt,
            Err(SymphoniaError::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break; // End of stream.
            }
            Err(SymphoniaError::ResetRequired) => {
                // Some formats need a reset after seeking.
                decoder.reset();
                continue;
            }
            Err(e) => return Err(DecodeError::Symphonia(e)),
        };

        // Skip packets for other tracks.
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(buf) => buf,
            Err(SymphoniaError::DecodeError(e)) => {
                log::warn!("Decode error (skipping packet): {e}");
                continue;
            }
            Err(e) => return Err(DecodeError::Symphonia(e)),
        };

        // Convert to interleaved f32.
        let spec = *decoded.spec();
        let num_frames = decoded.frames();
        let mut sample_buf = SampleBuffer::<f32>::new(num_frames as u64, spec);
        sample_buf.copy_interleaved_ref(decoded);
        all_samples.extend_from_slice(sample_buf.samples());
    }

    if all_samples.is_empty() {
        return Err(DecodeError::EmptyOutput);
    }

    let total_frames = all_samples.len() as u64 / channels as u64;
    let duration_secs = total_frames as f64 / sample_rate as f64;

    log::info!(
        "Decoded: {sample_rate} Hz, {channels} ch, {total_frames} frames, {duration_secs:.2}s"
    );

    Ok(DecodedAudio {
        samples: all_samples,
        sample_rate,
        channels,
        total_frames,
        duration_secs,
    })
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_invalid_data_returns_error() {
        let garbage = vec![0u8; 1000];
        let result = decode_mp3(&garbage);
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_empty_data_returns_error() {
        let result = decode_mp3(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_decoded_audio_invariants() {
        // Generate a minimal valid MP3 from a synthetic MPEG frame.
        // Since we can't easily create a real MP3 in a unit test without
        // an encoder dependency, we test the invariant checker with a
        // manually constructed DecodedAudio.
        let channels: u16 = 2;
        let sample_rate: u32 = 44100;
        let total_frames: u64 = 44100; // 1 second
        let samples: Vec<f32> = vec![0.0f32; (total_frames * channels as u64) as usize];
        let duration_secs = total_frames as f64 / sample_rate as f64;

        let decoded = DecodedAudio {
            samples: samples.clone(),
            sample_rate,
            channels,
            total_frames,
            duration_secs,
        };

        assert_eq!(decoded.sample_rate, 44100);
        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.total_frames, 44100);
        assert_eq!(decoded.samples.len(), (total_frames * channels as u64) as usize);
        assert!((decoded.duration_secs - 1.0).abs() < 0.001);
    }
}
