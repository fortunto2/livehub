//! Mic level meter + optional monitoring (listen). Opens a chosen INPUT device to
//! meter its peak, plus an output stream on the system default; while `listen` is on
//! it plays the mic out so you can confirm "is the mic actually audible?" from beam —
//! e.g. before/while streaming.
//!
//! ⚠️ Use HEADPHONES while monitoring: speakers + an open mic = a feedback loop.
//!
//! Pick a mic device DIFFERENT from the broadcast source — one capture stream per
//! device at a time (same gotcha as `LevelMonitor`).

use anyhow::{Context, Result};
use atomic_float::AtomicF32;
use cpal::traits::{DeviceTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::device::{best_input_config, best_output_config, pick_input, pick_output};

pub struct MicMonitor {
    _in: cpal::Stream,
    _out: cpal::Stream,
    level: Arc<AtomicF32>,
    listen: Arc<AtomicBool>,
    gain: Arc<AtomicF32>,
}

impl MicMonitor {
    /// Start metering `mic_hint`; the monitor output runs silent until `set_listen`.
    pub fn start(mic_hint: &str) -> Result<MicMonitor> {
        let in_dev = pick_input(mic_hint)?;
        let in_cfg = best_input_config(&in_dev)?;
        let in_channels = in_cfg.channels().max(1) as usize;
        let in_stream_cfg: cpal::StreamConfig = in_cfg.into();

        // Wait-free mic→speaker ring (mono); ~0.2 s at 48k absorbs callback jitter.
        let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(9600);
        let level = Arc::new(AtomicF32::new(0.0));
        let listen = Arc::new(AtomicBool::new(false));
        let gain = Arc::new(AtomicF32::new(1.0));

        let cb_level = level.clone();
        let in_stream = in_dev
            .build_input_stream(
                &in_stream_cfg,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let mut peak = 0.0f32;
                    let mut i = 0;
                    while i + in_channels <= data.len() {
                        let mut sum = 0.0f32; // downmix the frame to mono
                        for c in 0..in_channels {
                            sum += data[i + c];
                        }
                        let m = sum / in_channels as f32;
                        peak = peak.max(m.abs());
                        let _ = producer.push(m); // full (output not draining) → drop
                        i += in_channels;
                    }
                    let prev = cb_level.load(Ordering::Relaxed);
                    let v = if peak > prev { peak } else { prev * 0.85 };
                    cb_level.store(v.min(1.0), Ordering::Relaxed);
                },
                |_| {},
                None,
            )
            .context("build_input_stream (mic monitor)")?;

        let out_dev = pick_output("")?; // system default output (your headphones)
        let out_cfg = best_output_config(&out_dev)?;
        let out_channels = out_cfg.channels().max(1) as usize;
        let out_stream_cfg: cpal::StreamConfig = out_cfg.into();
        let cb_listen = listen.clone();
        let cb_gain = gain.clone();
        let out_stream = out_dev
            .build_output_stream(
                &out_stream_cfg,
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let on = cb_listen.load(Ordering::Relaxed);
                    let g = cb_gain.load(Ordering::Relaxed);
                    let mut i = 0;
                    while i + out_channels <= out.len() {
                        // Always drain the ring (even muted) so it can't overflow and
                        // stall the metering; only emit sound when listening.
                        let s = consumer.pop().unwrap_or(0.0);
                        let v = if on { (s * g).clamp(-1.0, 1.0) } else { 0.0 };
                        for c in 0..out_channels {
                            out[i + c] = v;
                        }
                        i += out_channels;
                    }
                },
                |_| {},
                None,
            )
            .context("build_output_stream (mic monitor)")?;

        in_stream.play().context("play mic input")?;
        out_stream.play().context("play mic monitor output")?;
        Ok(MicMonitor { _in: in_stream, _out: out_stream, level, listen, gain })
    }

    pub fn level(&self) -> f32 {
        self.level.load(Ordering::Relaxed)
    }
    pub fn set_listen(&self, on: bool) {
        self.listen.store(on, Ordering::Relaxed);
    }
    pub fn listening(&self) -> bool {
        self.listen.load(Ordering::Relaxed)
    }
    pub fn set_gain(&self, g: f32) {
        self.gain.store(g.clamp(0.0, 2.0), Ordering::Relaxed);
    }
}
