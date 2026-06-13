//! M1 audio engine: one deck + live mic mixed to the master, with a
//! sample-accurate atomic playhead.
//!
//! Two cpal streams on the SAME device (shared clock): an input stream pushes
//! the mic into an SPSC ring, the output stream mixes deck + mic and writes the
//! master pair. The deck track is decoded whole into a `TrackBuf` and swapped in
//! lock-free via `ArcSwapOption`; the audio callback reads it at an atomic frame
//! index. The callback stays alloc/lock-free (the SuperDuper `process()` rules).

use anyhow::{anyhow, Context, Result};
use arc_swap::ArcSwapOption;
use atomic_float::AtomicF32;
use cpal::traits::{DeviceTrait, StreamTrait};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use livehub_core::OutputRouting;

use crate::decoder::decode_track;
use crate::device::{best_input_config, best_output_config, pick_input, pick_output};
use crate::track::TrackBuf;

struct Shared {
    track: ArcSwapOption<TrackBuf>,
    pos: AtomicUsize,        // current deck frame, advanced by the audio thread
    seek_to: AtomicI64,      // UI-requested target frame, or -1
    playing: AtomicBool,
    loading: AtomicBool,     // a background decode is in flight
    deck_gain: AtomicF32,
    mic_gain: AtomicF32,
    aux_gain: AtomicF32,
    underruns: AtomicU64,
    seek_epoch: AtomicU64, // bumped on every discontinuous jump (seek / load)
    device_sr: u32,
    out_ch: usize,
    routing: OutputRouting,
    aux: Option<(usize, usize)>,
}

/// Owns the live audio streams and the shared transport state.
pub struct Engine {
    _in_stream: cpal::Stream,
    _out_stream: cpal::Stream,
    shared: Arc<Shared>,
}

/// Cheap `Send + Sync` read handle to the live transport, for off-thread
/// consumers (e.g. the video sync worker). `Engine` itself is `!Send` (it owns
/// the `cpal::Stream`s), so a worker thread takes a `Transport` instead. Clone
/// freely; all reads are wait-free atomic loads.
#[derive(Clone)]
pub struct Transport {
    shared: Arc<Shared>,
}

impl Transport {
    /// Current audio playhead in seconds.
    pub fn playhead_secs(&self) -> f64 {
        self.shared.pos.load(Ordering::Relaxed) as f64 / self.shared.device_sr as f64
    }
    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }
    /// Monotonic counter; a change since the last read means the playhead jumped
    /// discontinuously (user seek / track load / loop wrap) → re-seek the video.
    pub fn seek_epoch(&self) -> u64 {
        self.shared.seek_epoch.load(Ordering::Acquire)
    }
}

impl Engine {
    /// Build the engine on the named device (mic input + master output, plus an
    /// optional stereo `aux` input read from the same device's channels — e.g.
    /// the aggregate's BlackHole channels carrying other apps' audio).
    pub fn new(
        device_hint: &str,
        routing: OutputRouting,
        aux: Option<(usize, usize)>,
    ) -> Result<Self> {
        let out_dev = pick_output(device_hint)?;
        let in_dev = pick_input(device_hint)?;
        let out_cfg = best_output_config(&out_dev)?;
        let in_cfg = best_input_config(&in_dev)?;

        let out_ch = out_cfg.channels() as usize;
        let in_ch = in_cfg.channels() as usize;
        let device_sr = out_cfg.sample_rate().0;
        eprintln!(
            "engine: out '{}' {out_ch}ch @ {device_sr}Hz | in '{}' {in_ch}ch @ {}Hz",
            out_dev.name().unwrap_or_default(),
            in_dev.name().unwrap_or_default(),
            in_cfg.sample_rate().0
        );
        if routing.master_l >= out_ch || routing.master_r >= out_ch {
            return Err(anyhow!("master channels out of range for a {out_ch}ch device"));
        }
        if let Some((al, ar)) = aux {
            if al >= in_ch || ar >= in_ch {
                return Err(anyhow!("aux channels {al}/{ar} out of range for a {in_ch}ch input"));
            }
        }

        let shared = Arc::new(Shared {
            track: ArcSwapOption::empty(),
            pos: AtomicUsize::new(0),
            seek_to: AtomicI64::new(-1),
            playing: AtomicBool::new(false),
            loading: AtomicBool::new(false),
            deck_gain: AtomicF32::new(1.0),
            mic_gain: AtomicF32::new(1.0),
            aux_gain: AtomicF32::new(1.0),
            underruns: AtomicU64::new(0),
            seek_epoch: AtomicU64::new(0),
            device_sr,
            out_ch,
            routing,
            aux,
        });

        // Mic ring + optional aux ring: input callback (producers) -> output.
        let mic_ch = 0usize;
        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(device_sr as usize * 2);
        let (mut aux_prod, mut aux_cons) = rtrb::RingBuffer::<f32>::new(device_sr as usize * 2);

        let in_stream_cfg: cpal::StreamConfig = in_cfg.into();
        let in_stream = in_dev
            .build_input_stream(
                &in_stream_cfg,
                move |inp: &[f32], _: &cpal::InputCallbackInfo| {
                    let frames = inp.len() / in_ch;
                    for f in 0..frames {
                        let base = f * in_ch;
                        let m = inp.get(base + mic_ch).copied().unwrap_or(0.0);
                        let _ = prod.push(m);
                        let _ = prod.push(m);
                        if let Some((al, ar)) = aux {
                            let l = inp.get(base + al).copied().unwrap_or(0.0);
                            let r = inp.get(base + ar).copied().unwrap_or(0.0);
                            let _ = aux_prod.push(l);
                            let _ = aux_prod.push(r);
                        }
                    }
                },
                |e| eprintln!("input stream error: {e}"),
                None,
            )
            .context("build_input_stream")?;

        let sh = shared.clone();
        let out_stream_cfg: cpal::StreamConfig = out_cfg.into();
        let out_stream = out_dev
            .build_output_stream(
                &out_stream_cfg,
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    // Apply a pending seek at the block boundary.
                    let s = sh.seek_to.swap(-1, Ordering::AcqRel);
                    if s >= 0 {
                        sh.pos.store(s as usize, Ordering::Relaxed);
                    }
                    let track = sh.track.load();
                    let playing = sh.playing.load(Ordering::Relaxed);
                    let dg = sh.deck_gain.load(Ordering::Relaxed);
                    let mg = sh.mic_gain.load(Ordering::Relaxed);
                    let ag = sh.aux_gain.load(Ordering::Relaxed);
                    let out_ch = sh.out_ch;
                    let (ml, mr) = (sh.routing.master_l, sh.routing.master_r);
                    let frames = out.len() / out_ch;
                    let mut p = sh.pos.load(Ordering::Relaxed);

                    for f in 0..frames {
                        let base = f * out_ch;
                        for c in 0..out_ch {
                            if let Some(slot) = out.get_mut(base + c) {
                                *slot = 0.0;
                            }
                        }
                        let mic_l = cons.pop().unwrap_or_else(|_| {
                            sh.underruns.fetch_add(1, Ordering::Relaxed);
                            0.0
                        });
                        let mic_r = cons.pop().unwrap_or(0.0);
                        let aux_l = aux_cons.pop().unwrap_or(0.0);
                        let aux_r = aux_cons.pop().unwrap_or(0.0);

                        let (mut dl, mut dr) = (0.0_f32, 0.0_f32);
                        if playing {
                            if let Some(t) = track.as_ref() {
                                if let (Some(&a), Some(&b)) =
                                    (t.samples.get(p * 2), t.samples.get(p * 2 + 1))
                                {
                                    dl = a;
                                    dr = b;
                                    p += 1;
                                }
                            }
                        }

                        let l = dl * dg + mic_l * mg + aux_l * ag;
                        let r = dr * dg + mic_r * mg + aux_r * ag;
                        if let Some(slot) = out.get_mut(base + ml) {
                            *slot = l;
                        }
                        if let Some(slot) = out.get_mut(base + mr) {
                            *slot = r;
                        }
                    }
                    sh.pos.store(p, Ordering::Relaxed);
                },
                |e| eprintln!("output stream error: {e}"),
                None,
            )
            .context("build_output_stream")?;

        in_stream.play().context("input stream play")?;
        out_stream.play().context("output stream play")?;

        Ok(Self { _in_stream: in_stream, _out_stream: out_stream, shared })
    }

    /// Decode a file (on the calling thread) and load it into the deck at pos 0.
    /// Returns the track duration in seconds.
    pub fn load_track(&self, path: &Path) -> Result<f64> {
        let track = decode_track(path, self.shared.device_sr)?;
        let secs = track.duration_secs();
        self.shared.pos.store(0, Ordering::Relaxed);
        self.shared.seek_to.store(-1, Ordering::Relaxed);
        self.shared.seek_epoch.fetch_add(1, Ordering::Release);
        self.shared.track.store(Some(Arc::new(track)));
        Ok(secs)
    }

    /// Decode `path` on a background thread, then swap it into the deck. Returns
    /// immediately; poll [`is_loading`](Self::is_loading) for completion. The mic
    /// keeps passing through while decoding.
    pub fn load_track_background(&self, path: PathBuf) {
        let shared = self.shared.clone();
        std::thread::spawn(move || {
            shared.loading.store(true, Ordering::Relaxed);
            match decode_track(&path, shared.device_sr) {
                Ok(track) => {
                    shared.playing.store(false, Ordering::Relaxed);
                    shared.pos.store(0, Ordering::Relaxed);
                    shared.seek_to.store(-1, Ordering::Relaxed);
                    shared.seek_epoch.fetch_add(1, Ordering::Release);
                    shared.track.store(Some(Arc::new(track)));
                }
                Err(e) => eprintln!("load failed: {e:#}"),
            }
            shared.loading.store(false, Ordering::Relaxed);
        });
    }

    pub fn is_loading(&self) -> bool {
        self.shared.loading.load(Ordering::Relaxed)
    }

    pub fn play(&self) {
        self.shared.playing.store(true, Ordering::Relaxed);
    }
    pub fn pause(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
    }
    pub fn toggle(&self) -> bool {
        let next = !self.shared.playing.load(Ordering::Relaxed);
        self.shared.playing.store(next, Ordering::Relaxed);
        next
    }
    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }
    pub fn seek_secs(&self, secs: f64) {
        let frame = (secs.max(0.0) * self.shared.device_sr as f64) as i64;
        self.shared.seek_to.store(frame, Ordering::Relaxed);
        self.shared.seek_epoch.fetch_add(1, Ordering::Release);
    }
    pub fn set_deck_gain(&self, g: f32) {
        self.shared.deck_gain.store(g, Ordering::Relaxed);
    }
    pub fn set_mic_gain(&self, g: f32) {
        self.shared.mic_gain.store(g, Ordering::Relaxed);
    }
    pub fn set_aux_gain(&self, g: f32) {
        self.shared.aux_gain.store(g, Ordering::Relaxed);
    }
    pub fn has_aux(&self) -> bool {
        self.shared.aux.is_some()
    }
    pub fn playhead_secs(&self) -> f64 {
        self.shared.pos.load(Ordering::Relaxed) as f64 / self.shared.device_sr as f64
    }
    pub fn duration_secs(&self) -> f64 {
        self.shared
            .track
            .load()
            .as_ref()
            .map(|t| t.duration_secs())
            .unwrap_or(0.0)
    }
    /// Snapshot the currently loaded deck track (cheap `Arc` clone) for the GUI
    /// to read `waveform`/`beats`/`downbeats`/`bpm`. `None` if nothing is loaded.
    pub fn current_track(&self) -> Option<Arc<TrackBuf>> {
        self.shared.track.load_full()
    }
    /// Detected tempo of the loaded track, or `0.0` if none.
    pub fn bpm(&self) -> f64 {
        self.shared
            .track
            .load()
            .as_ref()
            .map(|t| t.bpm)
            .unwrap_or(0.0)
    }
    pub fn device_sr(&self) -> u32 {
        self.shared.device_sr
    }
    /// A cloneable, thread-safe read handle to the transport, for off-thread
    /// consumers (the video sync worker).
    pub fn transport(&self) -> Transport {
        Transport { shared: self.shared.clone() }
    }
    pub fn underruns(&self) -> u64 {
        self.shared.underruns.load(Ordering::Relaxed)
    }
}
