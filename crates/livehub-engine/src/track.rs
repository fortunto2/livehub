//! Decoded, device-rate audio for one deck: interleaved stereo f32 held whole in
//! memory (the M1 model — like sdsp-runner). The RT thread reads it at an atomic
//! frame index; no streaming ring needed at this stage.

pub struct TrackBuf {
    /// Interleaved stereo (L,R,L,R,…) at `sample_rate`.
    pub samples: Vec<f32>,
    /// Frame count (`samples.len() / 2`).
    pub frames: usize,
    /// Sample rate — equals the audio device rate (resampled on load).
    pub sample_rate: u32,
    /// Per-bin `(min, max)` peak envelope (mono, in `-1..=1`) for the waveform
    /// strip. ~2000 bins over the whole track; empty if not analysed.
    pub waveform: Vec<(f32, f32)>,
    /// Beat onset times in seconds (from `video_audio::beats::detect_beats`).
    pub beats: Vec<f64>,
    /// Downbeat (bar-start) times in seconds — a subset of `beats`.
    pub downbeats: Vec<f64>,
    /// Estimated tempo in BPM, or `0.0` if none was detected.
    pub bpm: f64,
}

impl TrackBuf {
    pub fn duration_secs(&self) -> f64 {
        self.frames as f64 / self.sample_rate as f64
    }
}
