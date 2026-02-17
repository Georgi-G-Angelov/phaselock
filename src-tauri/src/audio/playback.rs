use crate::audio::decoder::DecodedAudio;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Stream;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use parking_lot::Mutex;

// ── Playback state constants ────────────────────────────────────────────────

const STATE_STOPPED: u8 = 0;
const STATE_WAITING: u8 = 1;
const STATE_PLAYING: u8 = 2;
const STATE_PAUSED: u8 = 3;

// ── PlaybackStateEnum ───────────────────────────────────────────────────────

/// Human-readable playback state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackStateEnum {
    Stopped,
    Waiting,
    Playing,
    Paused,
}

impl From<u8> for PlaybackStateEnum {
    fn from(v: u8) -> Self {
        match v {
            STATE_STOPPED => Self::Stopped,
            STATE_WAITING => Self::Waiting,
            STATE_PLAYING => Self::Playing,
            STATE_PAUSED => Self::Paused,
            _ => Self::Stopped,
        }
    }
}

// ── PlaybackError ───────────────────────────────────────────────────────────

/// Errors that can occur when setting up audio output.
#[derive(Debug)]
pub enum PlaybackError {
    NoDevice,
    NoSupportedConfig,
    StreamError(String),
}

impl std::fmt::Display for PlaybackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDevice => write!(f, "No audio output device detected"),
            Self::NoSupportedConfig => write!(f, "No supported audio config found"),
            Self::StreamError(e) => write!(f, "Audio stream error: {e}"),
        }
    }
}

impl std::error::Error for PlaybackError {}

// ── PlaybackState (shared with audio callback) ─────────────────────────────

/// Shared state between the audio callback thread and the main thread.
pub struct PlaybackState {
    /// Interleaved f32 samples (Mutex for safe swapping from main thread).
    pub buffer: Mutex<Arc<Vec<f32>>>,
    /// Sample rate of the loaded track.
    pub sample_rate: std::sync::atomic::AtomicU32,
    /// Number of channels.
    pub channels: std::sync::atomic::AtomicU16,
    /// When to start playback (used in Waiting state).
    pub play_at: Mutex<Option<Instant>>,
    /// Current sample index (interleaved, so this counts individual samples).
    pub position: AtomicUsize,
    /// Playback state: 0=Stopped, 1=Waiting, 2=Playing, 3=Paused.
    pub state: AtomicU8,
    /// Volume multiplier (0.0–1.0). Stored as u32 (f32 bits).
    volume_bits: std::sync::atomic::AtomicU32,
    /// Callback invoked when the track finishes (transitions to Stopped).
    pub on_finished: Mutex<Option<Box<dyn Fn() + Send + 'static>>>,
}

impl PlaybackState {
    fn new() -> Self {
        Self {
            buffer: Mutex::new(Arc::new(Vec::new())),
            sample_rate: std::sync::atomic::AtomicU32::new(44100),
            channels: std::sync::atomic::AtomicU16::new(2),
            play_at: Mutex::new(None),
            position: AtomicUsize::new(0),
            state: AtomicU8::new(STATE_STOPPED),
            volume_bits: std::sync::atomic::AtomicU32::new(f32::to_bits(1.0)),
            on_finished: Mutex::new(None),
        }
    }

    /// Get the current channel count.
    pub fn get_channels(&self) -> u16 {
        self.channels.load(Ordering::Acquire)
    }

    fn volume(&self) -> f32 {
        f32::from_bits(self.volume_bits.load(Ordering::Relaxed))
    }

    fn set_volume_val(&self, v: f32) {
        self.volume_bits
            .store(f32::to_bits(v.clamp(0.0, 1.0)), Ordering::Relaxed);
    }
}

// ── AudioOutput ─────────────────────────────────────────────────────────────

/// Audio output manager. Owns the cpal stream and shared playback state.
pub struct AudioOutput {
    _stream: Stream,
    state: Arc<PlaybackState>,
    device_sample_rate: u32,
    device_channels: u16,
}

impl AudioOutput {
    /// Initialize audio output using the default device.
    pub fn new() -> Result<Self, PlaybackError> {
        let host = cpal::default_host();

        let device = host
            .default_output_device()
            .ok_or(PlaybackError::NoDevice)?;

        let device_name = device.name().unwrap_or_else(|_| "unknown".into());
        log::info!("Audio output device: {device_name}");

        let supported_config = device
            .default_output_config()
            .map_err(|e| PlaybackError::StreamError(format!("default config: {e}")))?;

        let device_sample_rate = supported_config.sample_rate().0;
        let device_channels = supported_config.channels();

        log::info!(
            "Audio config: {} Hz, {} channels, {:?}",
            device_sample_rate,
            device_channels,
            supported_config.sample_format()
        );

        let state = Arc::new(PlaybackState::new());
        state.channels.store(device_channels, Ordering::Release);

        let playback = state.clone();

        // Build the output stream.
        let config = cpal::StreamConfig {
            channels: device_channels,
            sample_rate: cpal::SampleRate(device_sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    Self::audio_callback(&playback, data);
                },
                |err| {
                    log::error!("Audio stream error: {err}");
                },
                None, // no timeout
            )
            .map_err(|e| PlaybackError::StreamError(format!("{e}")))?;

        stream
            .play()
            .map_err(|e| PlaybackError::StreamError(format!("play: {e}")))?;

        Ok(Self {
            _stream: stream,
            state,
            device_sample_rate,
            device_channels,
        })
    }

    /// The audio callback — runs on the audio thread.
    fn audio_callback(state: &PlaybackState, data: &mut [f32]) {
        let current_state = state.state.load(Ordering::Acquire);
        let volume = state.volume();

        match current_state {
            STATE_STOPPED | STATE_PAUSED => {
                // Write silence.
                for sample in data.iter_mut() {
                    *sample = 0.0;
                }
            }

            STATE_WAITING => {
                let should_play = {
                    let play_at = state.play_at.lock();
                    play_at.map_or(false, |t| Instant::now() >= t)
                };

                if should_play {
                    state.state.store(STATE_PLAYING, Ordering::Release);
                    Self::fill_samples(state, data, volume);
                } else {
                    for sample in data.iter_mut() {
                        *sample = 0.0;
                    }
                }
            }

            STATE_PLAYING => {
                Self::fill_samples(state, data, volume);
            }

            _ => {
                for sample in data.iter_mut() {
                    *sample = 0.0;
                }
            }
        }
    }

    /// Fill the output buffer with samples from the playback buffer.
    fn fill_samples(state: &PlaybackState, data: &mut [f32], volume: f32) {
        let buffer = state.buffer.lock().clone();
        let buf_len = buffer.len();

        if buf_len == 0 {
            for sample in data.iter_mut() {
                *sample = 0.0;
            }
            return;
        }

        let mut pos = state.position.load(Ordering::Acquire);
        let mut finished = false;

        for sample in data.iter_mut() {
            if pos < buf_len {
                *sample = buffer[pos] * volume;
                pos += 1;
            } else {
                *sample = 0.0;
                finished = true;
            }
        }

        state.position.store(pos, Ordering::Release);

        if finished {
            state.state.store(STATE_STOPPED, Ordering::Release);
            // Fire the on-finished callback (if set).
            let cb = state.on_finished.lock();
            if let Some(ref callback) = *cb {
                callback();
            }
        }
    }

    /// Load a decoded track into the playback buffer.
    /// Stops any current playback first.
    pub fn load_track(&self, decoded: DecodedAudio) {
        self.state.state.store(STATE_STOPPED, Ordering::Release);
        self.state.position.store(0, Ordering::Release);

        if decoded.sample_rate != self.device_sample_rate {
            log::warn!(
                "Sample rate mismatch: track={} Hz, device={} Hz — playback may sound wrong",
                decoded.sample_rate,
                self.device_sample_rate
            );
        }

        if decoded.channels != self.device_channels {
            log::warn!(
                "Channel count mismatch: track={}, device={} — playback may sound wrong",
                decoded.channels,
                self.device_channels
            );
        }

        // Safely swap the buffer, sample rate, and channels using interior mutability.
        *self.state.buffer.lock() = Arc::new(decoded.samples);
        self.state.sample_rate.store(decoded.sample_rate, Ordering::Release);
        self.state.channels.store(decoded.channels, Ordering::Release);
        log::info!(
            "Loaded track: {} Hz, {} ch, {} frames",
            decoded.sample_rate,
            decoded.channels,
            decoded.total_frames
        );
    }

    /// Schedule playback to start at a precise instant.
    pub fn play_at(&self, instant: Instant) {
        self.state.position.store(0, Ordering::Release);
        *self.state.play_at.lock() = Some(instant);
        self.state.state.store(STATE_WAITING, Ordering::Release);
    }

    /// Pause playback, keeping the current position.
    pub fn pause(&self) {
        self.state.state.store(STATE_PAUSED, Ordering::Release);
    }

    /// Resume playback from the current position at a precise instant.
    pub fn resume_at(&self, instant: Instant) {
        *self.state.play_at.lock() = Some(instant);
        self.state.state.store(STATE_WAITING, Ordering::Release);
    }

    /// Stop playback and reset position to 0.
    pub fn stop(&self) {
        self.state.state.store(STATE_STOPPED, Ordering::Release);
        self.state.position.store(0, Ordering::Release);
    }

    /// Seek to a specific sample offset (in frames).
    pub fn seek(&self, position_frames: u64) {
        let channels = self.state.get_channels() as u64;
        let sample_index = position_frames * channels;
        self.state
            .position
            .store(sample_index as usize, Ordering::Release);
    }

    /// Get the current playback position in frames.
    pub fn get_position(&self) -> u64 {
        let pos = self.state.position.load(Ordering::Acquire) as u64;
        let channels = self.state.get_channels() as u64;
        if channels == 0 {
            return 0;
        }
        pos / channels
    }

    /// Get the current playback state.
    pub fn get_state(&self) -> PlaybackStateEnum {
        PlaybackStateEnum::from(self.state.state.load(Ordering::Acquire))
    }

    /// Set the volume (0.0–1.0).
    pub fn set_volume(&self, volume: f32) {
        self.state.set_volume_val(volume);
    }

    /// Register a callback for when the track finishes playing.
    pub fn on_track_finished<F: Fn() + Send + 'static>(&self, callback: F) {
        *self.state.on_finished.lock() = Some(Box::new(callback));
    }

    /// Get a clone of the shared playback state (for testing / external access).
    pub fn playback_state(&self) -> Arc<PlaybackState> {
        self.state.clone()
    }
}

// ── Standalone PlaybackState tests (no audio device needed) ─────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a PlaybackState with a test buffer for state transition tests.
    fn make_test_state(num_frames: usize, channels: u16) -> Arc<PlaybackState> {
        let samples = vec![0.5f32; num_frames * channels as usize];
        let state = PlaybackState::new();
        *state.buffer.lock() = Arc::new(samples);
        state.sample_rate.store(44100, Ordering::Release);
        state.channels.store(channels, Ordering::Release);
        Arc::new(state)
    }

    #[test]
    fn test_initial_state_is_stopped() {
        let state = make_test_state(100, 2);
        assert_eq!(
            PlaybackStateEnum::from(state.state.load(Ordering::Acquire)),
            PlaybackStateEnum::Stopped
        );
    }

    #[test]
    fn test_state_transitions_waiting_to_playing() {
        let state = make_test_state(1000, 2);

        // Set play_at to now (in the past).
        *state.play_at.lock() = Some(Instant::now() - std::time::Duration::from_secs(1));
        state.state.store(STATE_WAITING, Ordering::Release);

        // Simulate a callback.
        let mut output = vec![0.0f32; 64];
        AudioOutput::audio_callback(&state, &mut output);

        // Should have transitioned to Playing.
        assert_eq!(
            PlaybackStateEnum::from(state.state.load(Ordering::Acquire)),
            PlaybackStateEnum::Playing
        );

        // Output should have non-zero samples.
        assert!(output.iter().any(|s| *s != 0.0));
    }

    #[test]
    fn test_state_waiting_future_writes_silence() {
        let state = make_test_state(1000, 2);

        // Set play_at far in the future.
        *state.play_at.lock() = Some(Instant::now() + std::time::Duration::from_secs(3600));
        state.state.store(STATE_WAITING, Ordering::Release);

        let mut output = vec![1.0f32; 64];
        AudioOutput::audio_callback(&state, &mut output);

        // Should still be Waiting.
        assert_eq!(
            PlaybackStateEnum::from(state.state.load(Ordering::Acquire)),
            PlaybackStateEnum::Waiting
        );

        // Output should be silence.
        assert!(output.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn test_playing_fills_samples_and_advances_position() {
        let state = make_test_state(100, 2);
        state.state.store(STATE_PLAYING, Ordering::Release);
        state.position.store(0, Ordering::Release);

        let mut output = vec![0.0f32; 20];
        AudioOutput::audio_callback(&state, &mut output);

        // Position should have advanced by 20 samples.
        assert_eq!(state.position.load(Ordering::Acquire), 20);

        // All output samples should be 0.5 * 1.0 (volume).
        assert!(output.iter().all(|s| (*s - 0.5).abs() < 0.001));
    }

    #[test]
    fn test_playing_reaches_end_transitions_to_stopped() {
        // 10 frames * 2 channels = 20 samples total.
        let state = make_test_state(10, 2);
        state.state.store(STATE_PLAYING, Ordering::Release);
        state.position.store(0, Ordering::Release);

        // Request more samples than available.
        let mut output = vec![0.0f32; 40];
        AudioOutput::audio_callback(&state, &mut output);

        // Should have transitioned to Stopped.
        assert_eq!(
            PlaybackStateEnum::from(state.state.load(Ordering::Acquire)),
            PlaybackStateEnum::Stopped
        );

        // First 20 samples filled, rest silence.
        assert!(output[..20].iter().all(|s| (*s - 0.5).abs() < 0.001));
        assert!(output[20..].iter().all(|s| *s == 0.0));
    }

    #[test]
    fn test_paused_writes_silence() {
        let state = make_test_state(100, 2);
        state.state.store(STATE_PAUSED, Ordering::Release);
        state.position.store(50, Ordering::Release);

        let mut output = vec![1.0f32; 20];
        AudioOutput::audio_callback(&state, &mut output);

        // Output should be silence.
        assert!(output.iter().all(|s| *s == 0.0));

        // Position should NOT have changed.
        assert_eq!(state.position.load(Ordering::Acquire), 50);
    }

    #[test]
    fn test_stopped_writes_silence() {
        let state = make_test_state(100, 2);
        state.state.store(STATE_STOPPED, Ordering::Release);

        let mut output = vec![1.0f32; 20];
        AudioOutput::audio_callback(&state, &mut output);

        assert!(output.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn test_volume_scaling() {
        let state = make_test_state(100, 2);
        state.state.store(STATE_PLAYING, Ordering::Release);
        state.set_volume_val(0.5);

        let mut output = vec![0.0f32; 20];
        AudioOutput::audio_callback(&state, &mut output);

        // 0.5 (sample) * 0.5 (volume) = 0.25.
        assert!(output.iter().all(|s| (*s - 0.25).abs() < 0.001));
    }

    #[test]
    fn test_volume_clamping() {
        let state = make_test_state(100, 2);

        state.set_volume_val(2.0);
        assert!((state.volume() - 1.0).abs() < 0.001);

        state.set_volume_val(-0.5);
        assert!((state.volume() - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_on_finished_callback_fires() {
        let state = make_test_state(10, 2); // 20 samples total.
        state.state.store(STATE_PLAYING, Ordering::Release);

        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let finished_clone = finished.clone();
        *state.on_finished.lock() = Some(Box::new(move || {
            finished_clone.store(true, Ordering::Release);
        }));

        // Request more than available.
        let mut output = vec![0.0f32; 40];
        AudioOutput::audio_callback(&state, &mut output);

        assert!(finished.load(Ordering::Acquire));
        assert_eq!(
            PlaybackStateEnum::from(state.state.load(Ordering::Acquire)),
            PlaybackStateEnum::Stopped
        );
    }

    #[test]
    fn test_pause_resume_cycle() {
        let state = make_test_state(1000, 2);
        state.state.store(STATE_PLAYING, Ordering::Release);

        // Play some samples.
        let mut output = vec![0.0f32; 100];
        AudioOutput::audio_callback(&state, &mut output);
        let pos_after_play = state.position.load(Ordering::Acquire);
        assert_eq!(pos_after_play, 100);

        // Pause.
        state.state.store(STATE_PAUSED, Ordering::Release);
        AudioOutput::audio_callback(&state, &mut output);
        // Position unchanged.
        assert_eq!(state.position.load(Ordering::Acquire), 100);

        // Resume (set play_at in the past to trigger immediately).
        *state.play_at.lock() = Some(Instant::now() - std::time::Duration::from_secs(1));
        state.state.store(STATE_WAITING, Ordering::Release);
        AudioOutput::audio_callback(&state, &mut output);

        // Should have transitioned to Playing and advanced position.
        assert_eq!(
            PlaybackStateEnum::from(state.state.load(Ordering::Acquire)),
            PlaybackStateEnum::Playing
        );
        assert_eq!(state.position.load(Ordering::Acquire), 200);
    }

    #[test]
    fn test_seek_updates_position() {
        let state = make_test_state(1000, 2);
        state.state.store(STATE_PLAYING, Ordering::Release);

        // Seek to frame 500 (= sample index 1000 for stereo).
        let channels = state.get_channels() as u64;
        state.position.store((500 * channels) as usize, Ordering::Release);

        assert_eq!(state.position.load(Ordering::Acquire), 1000);

        // Play some samples from the new position.
        let mut output = vec![0.0f32; 20];
        AudioOutput::audio_callback(&state, &mut output);
        assert_eq!(state.position.load(Ordering::Acquire), 1020);
    }
}
