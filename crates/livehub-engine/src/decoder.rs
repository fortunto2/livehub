//! Offline track decode (symphonia) → interleaved stereo f32 at the device rate.
//! Runs on a worker / loader thread, never on the audio thread.
//!
//! Resampling is linear for M1 (good enough for testing; the common case —
//! 44.1 kHz file on a 44.1 kHz device — needs none). Upgrade to rubato later.

use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use video_core::beats::BeatConfig;

use crate::track::TrackBuf;

/// Target sample rate for beat detection. The detector is rate-agnostic
/// (`frames_per_sec = sample_rate / hop_length`), but ~22 kHz keeps the STFT
/// cheap while preserving onset transients — same rate its tests use.
const BEAT_SR: u32 = 22_050;

/// Target number of `(min, max)` peak bins for the whole-track waveform strip.
const WAVEFORM_BINS: usize = 2000;

pub fn decode_track(path: &Path, target_sr: u32) -> Result<TrackBuf> {
    // symphonia first (fast, pure-Rust); fall back to the ffmpeg CLI for the many
    // video-container audio codecs symphonia can't decode (AAC profiles, AC-3, …).
    let (left, right) = match decode_symphonia(path, target_sr) {
        Ok(lr) => lr,
        Err(e) => {
            eprintln!("decoder: symphonia failed ({e}); falling back to ffmpeg");
            decode_ffmpeg(path, target_sr)
                .with_context(|| format!("ffmpeg audio decode of {}", path.display()))?
        }
    };

    if left.is_empty() {
        bail!("decoded no audio from {}", path.display());
    }

    let frames = left.len().min(right.len());
    let mut samples = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        samples.push(left[i]);
        samples.push(right[i]);
    }

    // M2.5: beat/downbeat/bpm + a peak-envelope waveform. All off the audio
    // thread, on this worker — allocations are fine here.
    let mono = downmix_mono(&left, &right, frames);
    let (beats, downbeats, bpm) = analyze_beats(&mono, target_sr);
    let waveform = peak_envelope(&mono, WAVEFORM_BINS);

    Ok(TrackBuf {
        samples,
        frames,
        sample_rate: target_sr,
        waveform,
        beats,
        downbeats,
        bpm,
    })
}

/// Pure-Rust decode via symphonia → `(left, right)` planar at `target_sr`.
fn decode_symphonia(path: &Path, target_sr: u32) -> Result<(Vec<f32>, Vec<f32>)> {
    let file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .context("probing audio format")?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| anyhow!("no default audio track"))?;
    let track_id = track.id;
    let file_sr = track.codec_params.sample_rate.unwrap_or(44_100);
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .context("creating decoder")?;

    let mut left: Vec<f32> = Vec::new();
    let mut right: Vec<f32> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(_)) => break, // EOF
            Err(e) => return Err(anyhow!("reading packet: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphoniaError::DecodeError(_)) => continue, // skip a bad frame
            Err(e) => return Err(anyhow!("decode: {e}")),
        };
        let spec = *decoded.spec();
        let ch = spec.channels.count();
        if ch == 0 {
            continue;
        }
        let mut sb = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        sb.copy_interleaved_ref(decoded);
        for frame in sb.samples().chunks_exact(ch) {
            let (l, r) = if ch == 1 {
                (frame[0], frame[0])
            } else {
                (frame[0], frame[1])
            };
            left.push(l);
            right.push(r);
        }
    }

    if left.is_empty() {
        bail!("symphonia decoded no audio");
    }

    Ok(if file_sr != target_sr {
        (
            linear_resample(&left, file_sr, target_sr),
            linear_resample(&right, file_sr, target_sr),
        )
    } else {
        (left, right)
    })
}

/// Robust decode via the ffmpeg CLI → interleaved f32le stereo at `target_sr`,
/// de-interleaved to `(left, right)`. Handles any codec ffmpeg supports.
fn decode_ffmpeg(path: &Path, target_sr: u32) -> Result<(Vec<f32>, Vec<f32>)> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args([
            "-vn",
            "-f",
            "f32le",
            "-ac",
            "2",
            "-ar",
            &target_sr.to_string(),
            "pipe:1",
        ])
        .stderr(Stdio::null())
        .output()
        .context("spawning ffmpeg")?;
    if !out.status.success() {
        bail!("ffmpeg exited with {}", out.status);
    }
    let bytes = out.stdout;
    let frames = bytes.len() / 8; // 2 channels * 4 bytes
    let mut left = Vec::with_capacity(frames);
    let mut right = Vec::with_capacity(frames);
    for fr in bytes.chunks_exact(8) {
        left.push(f32::from_le_bytes([fr[0], fr[1], fr[2], fr[3]]));
        right.push(f32::from_le_bytes([fr[4], fr[5], fr[6], fr[7]]));
    }
    Ok((left, right))
}

/// Sum L+R to a mono signal averaged to `-1..=1`.
fn downmix_mono(left: &[f32], right: &[f32], frames: usize) -> Vec<f32> {
    let mut mono = Vec::with_capacity(frames);
    for i in 0..frames {
        mono.push((left[i] + right[i]) * 0.5);
    }
    mono
}

/// Run `video_audio::beats::detect_beats` on a `BEAT_SR`-decimated copy of the
/// mono signal. Returns `(beats, downbeats, bpm)` — all times in seconds.
fn analyze_beats(mono: &[f32], mono_sr: u32) -> (Vec<f64>, Vec<f64>, f64) {
    // Decimate to BEAT_SR, but never claim a higher rate than we actually have:
    // for an exotic sub-22050 device, decimate_to returns the input unchanged at
    // `mono_sr`, so the rate handed to the detector must follow suit.
    let beat_sr = mono_sr.min(BEAT_SR);
    let decimated = decimate_to(mono, mono_sr, beat_sr);
    if decimated.is_empty() {
        return (Vec::new(), Vec::new(), 0.0);
    }
    let result = video_audio::beats::detect_beats(&decimated, beat_sr, &BeatConfig::default());
    (result.beats, result.downbeats, result.bpm)
}

/// Nearest-sample decimation to a lower (or equal) rate — cheap and adequate
/// for onset/tempo analysis. Returns the input unchanged when `to >= from`.
fn decimate_to(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if input.is_empty() || from == 0 || to == 0 || to >= from {
        return input.to_vec();
    }
    let ratio = from as f64 / to as f64;
    let out_len = (input.len() as f64 / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = (i as f64 * ratio) as usize;
        out.push(input[src.min(input.len() - 1)]);
    }
    out
}

/// Bucket the mono signal into `bins` `(min, max)` peaks for the waveform strip.
fn peak_envelope(mono: &[f32], bins: usize) -> Vec<(f32, f32)> {
    if mono.is_empty() || bins == 0 {
        return Vec::new();
    }
    let bins = bins.min(mono.len());
    let mut out = Vec::with_capacity(bins);
    let per_bin = mono.len() as f64 / bins as f64;
    for b in 0..bins {
        let start = (b as f64 * per_bin) as usize;
        let end = (((b + 1) as f64 * per_bin) as usize).max(start + 1).min(mono.len());
        let mut lo = f32::INFINITY;
        let mut hi = f32::NEG_INFINITY;
        for &s in &mono[start..end] {
            lo = lo.min(s);
            hi = hi.max(s);
        }
        if lo.is_finite() && hi.is_finite() {
            out.push((lo, hi));
        } else {
            out.push((0.0, 0.0));
        }
    }
    out
}

/// Linear-interpolation resample (M1 placeholder; rubato later for quality).
fn linear_resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let ratio = to as f64 / from as f64;
    let out_len = (input.len() as f64 * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 / ratio;
        let i0 = src.floor() as usize;
        let frac = (src - i0 as f64) as f32;
        let s0 = input.get(i0).copied().unwrap_or(0.0);
        let s1 = input.get(i0 + 1).copied().unwrap_or(s0);
        out.push(s0 + (s1 - s0) * frac);
    }
    out
}
