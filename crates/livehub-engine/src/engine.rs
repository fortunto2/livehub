//! M4 audio engine: TWO independent decks + live mic mixed to the master, with
//! per-deck sample-accurate atomic playheads and an equal-power crossfader.
//!
//! Two cpal streams on the SAME device (shared clock): an input stream pushes
//! the mic into an SPSC ring, the output stream mixes both decks + mic and
//! writes the master pair. Each deck's track is decoded whole into a `TrackBuf`
//! and swapped in lock-free via `ArcSwapOption`; the audio callback reads it at
//! a per-deck atomic frame index, using only fixed `[_; 2]` stack arrays.
//! RT-callback rules: see crate docs (lib.rs).

use anyhow::{anyhow, Context, Result};
use arc_swap::ArcSwapOption;
use atomic_float::AtomicF32;
use core::f32::consts::FRAC_PI_2;
use cpal::traits::{DeviceTrait, StreamTrait};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use livehub_core::OutputRouting;

use crate::clap_fx::{list_superduper_clap, MasterFx, ParamInfo, ParamSet};
use crate::decoder::decode_track;
use crate::device::{best_input_config, best_output_config, pick_input, pick_output};
use crate::track::TrackBuf;

/// Largest output block we ever sub-block the master FX into. The cpal callback
/// hands variable-size buffers; we process the master FX in chunks of at most
/// this many frames so the plugin's activated `max_frames` is never exceeded.
const MAX_FX_BLOCK: usize = 2048;

/// Playback-rate clamp (two octaves either way). The ±8% beatmatch range sits
/// well inside this; the bound just keeps interpolation + the playhead advance
/// sane against a wild value.
const MIN_RATE: f32 = 0.25;
const MAX_RATE: f32 = 4.0;

/// Declick ramp length (output frames). After a discontinuous jump (seek or beat
/// loop wrap) the deck fades in linearly over this many samples (~2.7 ms at
/// 48 kHz) so the jump's edge doesn't pop. Short enough not to soften transients.
const DECLICK_SAMPLES: u32 = 128;

/// One independent deck: its own track, playhead, seek state, play flag and gain.
struct Deck {
    track: ArcSwapOption<TrackBuf>,
    /// FRACTIONAL deck frame (f64 bits in an AtomicU64), advanced by the audio
    /// thread by `rate` per output frame. Fractional so variable-rate (vinyl)
    /// playback reads between sample frames by linear interpolation. Store via
    /// `f64::to_bits`, read via `f64::from_bits`. `playhead_secs = posf / sr`.
    pos: AtomicU64,
    seek_to: AtomicI64,      // UI-requested target frame, or -1
    seek_epoch: AtomicU64,   // bumped on every discontinuous jump (seek / load)
    playing: AtomicBool,
    loading: AtomicBool,     // a background decode is in flight
    gain: AtomicF32,
    /// Playback rate: 1.0 = original speed/pitch, >1 = faster+higher (vinyl-style,
    /// NOT pitch-preserving). One Relaxed load per deck per block.
    rate: AtomicF32,
    loop_start: AtomicI64,   // beat-loop start frame, or -1 (no loop)
    loop_end: AtomicI64,     // beat-loop end frame (exclusive), or -1 (no loop)
    cue: AtomicBool,         // headphone CUE / PFL engaged for this deck
    declick: AtomicU32,      // frames left in the post-jump fade-in (0 = none)
}

impl Deck {
    fn new() -> Self {
        Deck {
            track: ArcSwapOption::empty(),
            pos: AtomicU64::new(0.0f64.to_bits()),
            seek_to: AtomicI64::new(-1),
            seek_epoch: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            loading: AtomicBool::new(false),
            gain: AtomicF32::new(1.0),
            rate: AtomicF32::new(1.0),
            loop_start: AtomicI64::new(-1),
            loop_end: AtomicI64::new(-1),
            cue: AtomicBool::new(false),
            declick: AtomicU32::new(0),
        }
    }
}

struct Shared {
    decks: [Deck; 2],
    crossfade: AtomicF32, // 0.0 = full deck A, 1.0 = full deck B
    mic_gain: AtomicF32,
    aux_gain: AtomicF32,
    cue_gain: AtomicF32,  // headphone CUE bus level (shared across cued decks)
    underruns: AtomicU64,
    device_sr: u32,
    out_ch: usize,
    routing: OutputRouting,
    aux: Option<(usize, usize)>,
    /// Mic + aux ring consumers, parked here so the OUTPUT stream can be rebuilt
    /// (to load/unload a master FX) without disturbing the INPUT stream or the
    /// rings. `build_output_stream` `.take()`s them into its callback; the next
    /// rebuild puts them back here when the old output stream's callback drops.
    /// The producers stay owned by the live input stream and never move.
    mic_cons: Mutex<Option<rtrb::Consumer<f32>>>,
    aux_cons: Mutex<Option<rtrb::Consumer<f32>>>,
}

/// Clamp a public deck index to the valid range `{0, 1}`.
#[inline]
fn deck_idx(deck: usize) -> usize {
    deck.min(1)
}

/// Owns the mic/aux ring consumers inside the output callback and returns them
/// to [`Shared`] when the callback is dropped (i.e. when the output stream is
/// torn down for an FX reload). This lets us rebuild ONLY the output stream:
/// the input stream + ring producers stay alive, and the next
/// `build_output_stream` re-`take()`s the parked consumers. The consumers are
/// single-consumer and not `Clone`, so this hand-back is how the new callback
/// gets them without recreating the rings.
struct RingLease {
    mic: Option<rtrb::Consumer<f32>>,
    aux: Option<rtrb::Consumer<f32>>,
    shared: Arc<Shared>,
}

impl Drop for RingLease {
    fn drop(&mut self) {
        // Park the consumers back in Shared for the next output-stream build.
        if let (Ok(mut m), Ok(mut a)) =
            (self.shared.mic_cons.lock(), self.shared.aux_cons.lock())
        {
            *m = self.mic.take();
            *a = self.aux.take();
        }
    }
}

/// Build the master OUTPUT stream: mixes both decks + mic + aux into the master
/// pair, optionally running the whole mix through a master-bus CLAP insert
/// (`fx`) just before it hits the device. The mic/aux ring consumers are
/// `.take()`n out of `shared` (parked there by [`Engine::new`] / the previous
/// stream's [`RingLease`] drop) and owned by the callback via a `RingLease`,
/// so this can be called repeatedly to load/unload an FX while the INPUT stream
/// and the rings stay alive.
///
/// The callback stays alloc/lock-free on the hot path: it fills a fixed stereo
/// scratch for the block, runs `fx.process()` (a wait-free rtrb drain + the
/// plugin's own `process()`), then scatters to the device channels. The only
/// lock is taken ONCE at build time (`mic_cons.lock()` to take the consumers),
/// never inside the audio callback.
fn build_output_stream(
    out_dev: &cpal::Device,
    out_cfg: &cpal::SupportedStreamConfig,
    shared: Arc<Shared>,
    mut fx: Option<MasterFx>,
) -> Result<cpal::Stream> {
    // Take the parked ring consumers; the RingLease returns them on drop.
    let mic = shared.mic_cons.lock().unwrap().take();
    let aux = shared.aux_cons.lock().unwrap().take();
    let mut lease = RingLease { mic, aux, shared: shared.clone() };

    // Block-sized stereo scratch for the master FX: the cpal buffer is variable
    // size, so we sub-block it to MAX_FX_BLOCK frames. Pre-allocated here (main
    // thread, at build time) — never touched if `fx` is None.
    let mut mbuf_l = vec![0.0f32; MAX_FX_BLOCK];
    let mut mbuf_r = vec![0.0f32; MAX_FX_BLOCK];
    // CUE / PFL scratch, same lifetime + capture as the master scratch. The cue
    // bus is PRE-crossfade and PRE-master-FX: it never runs through `fx`, so it
    // is filled in phase 1 and scattered straight to the headphone channels.
    let mut cbuf_l = vec![0.0f32; MAX_FX_BLOCK];
    let mut cbuf_r = vec![0.0f32; MAX_FX_BLOCK];

    let sh = shared;
    let out_stream_cfg: cpal::StreamConfig = out_cfg.clone().into();
    let stream = out_dev
        .build_output_stream(
            &out_stream_cfg,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                // The lease owns the consumers; touch them through it. (Keeping
                // `lease` captured is what makes the hand-back-on-drop work.)
                let (cons, aux_cons) = match (&mut lease.mic, &mut lease.aux) {
                    (Some(c), Some(a)) => (c, a),
                    _ => return,
                };

                // Per-deck: apply a pending seek at the block boundary, then
                // snapshot the track Arc / playhead / play flag / gain ONCE.
                // Fixed [_; 2] stack arrays only — no heap, no lock.
                let tracks = [sh.decks[0].track.load(), sh.decks[1].track.load()];
                // FRACTIONAL in-block playhead (f64), advanced by `rate` per frame.
                let mut p = [0.0f64; 2];
                let mut playing = [false; 2];
                let mut dg = [0.0f32; 2];
                // Per-deck playback rate (1.0 = original), snapshotted once/block.
                let mut rate = [1.0f64; 2];
                // Per-deck headphone CUE / PFL flag, snapshotted once per block.
                let mut cue = [false; 2];
                // Per-deck beat-loop region (frames); -1 = no loop. Snapshotted
                // once per block, used to wrap the playhead in the inner loop.
                let mut lstart = [-1i64; 2];
                let mut lend = [-1i64; 2];
                // Set true if a deck's playhead wrapped this block → bump its
                // seek_epoch ONCE after the block so its video re-seeks too.
                let mut wrapped = [false; 2];
                // Per-deck declick fade-in counter (frames left), carried across
                // blocks via the atomic; a seek/loop-wrap (re)arms it to fade the
                // jump edge in instead of popping.
                let mut declick = [0u32; 2];
                for d in 0..2 {
                    let s = sh.decks[d].seek_to.swap(-1, Ordering::AcqRel);
                    declick[d] = sh.decks[d].declick.load(Ordering::Relaxed);
                    if s >= 0 {
                        // Seek lands on an integer frame, stored as fractional bits.
                        sh.decks[d].pos.store((s as f64).to_bits(), Ordering::Relaxed);
                        declick[d] = DECLICK_SAMPLES; // fade the jump in, no pop.
                    }
                    p[d] = f64::from_bits(sh.decks[d].pos.load(Ordering::Relaxed));
                    playing[d] = sh.decks[d].playing.load(Ordering::Relaxed);
                    dg[d] = sh.decks[d].gain.load(Ordering::Relaxed);
                    rate[d] = sh.decks[d].rate.load(Ordering::Relaxed) as f64;
                    lstart[d] = sh.decks[d].loop_start.load(Ordering::Relaxed);
                    lend[d] = sh.decks[d].loop_end.load(Ordering::Relaxed);
                    cue[d] = sh.decks[d].cue.load(Ordering::Relaxed);
                }
                // Equal-power crossfade: cos/sin keeps constant power across
                // the sweep (no -6 dB center dip a linear fade would cause).
                let xf = sh.crossfade.load(Ordering::Relaxed).clamp(0.0, 1.0);
                let g_a = (xf * FRAC_PI_2).cos();
                let g_b = (xf * FRAC_PI_2).sin();
                let xw = [g_a, g_b];

                let mg = sh.mic_gain.load(Ordering::Relaxed);
                let ag = sh.aux_gain.load(Ordering::Relaxed);
                let out_ch = sh.out_ch;
                let (ml, mr) = (sh.routing.master_l, sh.routing.master_r);
                // CUE / PFL: a concrete (l, r) channel pair only when BOTH are
                // configured, else None (2ch device → no headphone pair → cue is
                // a no-op). `cg` is the shared headphone bus level.
                let cue_ch = sh.routing.cue_l.zip(sh.routing.cue_r);
                let cg = sh.cue_gain.load(Ordering::Relaxed);
                // Any deck cued this block? Skip the whole cue path otherwise so
                // an idle headphone bus costs nothing on the hot path.
                let any_cue = cue_ch.is_some() && (cue[0] || cue[1]);
                let frames = out.len() / out_ch;

                // Phase 1: fill the stereo master scratch (mbuf_l/mbuf_r) for the
                // whole block, sub-blocking to MAX_FX_BLOCK so the FX never sees
                // more than its activated max_frames. Decks + mic + aux summed
                // here, exactly as before; the device-channel scatter happens in
                // phase 2 (after the optional FX) so the FX processes the SAME
                // master mix that used to be written straight to the device.
                let mut done = 0usize;
                while done < frames {
                    let chunk = (frames - done).min(MAX_FX_BLOCK);
                    for i in 0..chunk {
                        let f = done + i;
                        let base = f * out_ch;
                        // Clear all device channels for this frame up front.
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
                        // CUE / PFL accumulator: each cued deck's RAW stereo with
                        // its own deck gain applied, but WITHOUT the crossfade
                        // weight (pre-fader listen) and WITHOUT the master FX.
                        let (mut cl, mut cr) = (0.0_f32, 0.0_f32);
                        for d in 0..2 {
                            if playing[d] {
                                if let Some(t) = tracks[d].as_ref() {
                                    // Variable-rate read: floor the fractional
                                    // playhead to the frame index, interpolate
                                    // linearly toward the NEXT frame by `frac`.
                                    // Both taps go through the bounds-guarded
                                    // `samples.get(..)`; out-of-range → 0.0, so
                                    // end-of-track is silent (the deck contributes
                                    // nothing once it runs off the buffer).
                                    let i0 = p[d].floor();
                                    let frac = (p[d] - i0) as f32;
                                    let i = i0 as usize;
                                    let g = |k: usize| t.samples.get(k).copied().unwrap_or(0.0);
                                    // Only mix while the current frame exists; past
                                    // the end both `get`s miss → silence + no advance.
                                    if i0 >= 0.0 && i * 2 + 1 < t.samples.len() {
                                        let a0 = g(i * 2);
                                        let a1 = g(i * 2 + 2);
                                        let b0 = g(i * 2 + 1);
                                        let b1 = g(i * 2 + 3);
                                        // Post-jump declick: linear fade-in 0→1 over
                                        // DECLICK_SAMPLES frames so a seek/wrap edge
                                        // doesn't pop. 1.0 once the counter drains.
                                        let ramp = if declick[d] > 0 {
                                            let r = 1.0
                                                - declick[d] as f32 / DECLICK_SAMPLES as f32;
                                            declick[d] -= 1;
                                            r
                                        } else {
                                            1.0
                                        };
                                        let a = (a0 + (a1 - a0) * frac) * ramp;
                                        let b = (b0 + (b1 - b0) * frac) * ramp;
                                        let w = dg[d] * xw[d];
                                        dl += a * w;
                                        dr += b * w;
                                        // PFL tap: deck-gain applied, crossfade NOT.
                                        if any_cue && cue[d] {
                                            cl += a * dg[d];
                                            cr += b * dg[d];
                                        }
                                        // Advance by the deck's rate (fractional).
                                        p[d] += rate[d];
                                        // Beat-loop wrap: a valid loop is ls>=0 &&
                                        // le>ls; on reaching the end, snap back to
                                        // start, preserving the fractional overshoot
                                        // for sample-accurate looping at non-unit
                                        // rates. Flag the wrap so we bump the epoch
                                        // once per block (not per sample).
                                        let (ls, le) = (lstart[d], lend[d]);
                                        if ls >= 0 && le > ls && p[d] >= le as f64 {
                                            p[d] = ls as f64 + (p[d] - le as f64);
                                            wrapped[d] = true;
                                            declick[d] = DECLICK_SAMPLES; // fade the seam.
                                        }
                                    }
                                }
                            }
                        }

                        mbuf_l[i] = dl + mic_l * mg + aux_l * ag;
                        mbuf_r[i] = dr + mic_r * mg + aux_r * ag;
                        // Cue bus: soloed deck(s) only, scaled by the shared
                        // headphone level. Filled here (NOT through `fx`).
                        if any_cue {
                            cbuf_l[i] = cl * cg;
                            cbuf_r[i] = cr * cg;
                        }
                    }

                    // The master-bus insert: process the whole mix in place.
                    if let Some(fx) = fx.as_mut() {
                        fx.process(&mut mbuf_l[..chunk], &mut mbuf_r[..chunk]);
                    }

                    // Phase 2: scatter the (post-FX) master pair to the device,
                    // plus the (FX-bypassed) cue pair to the headphone channels.
                    for i in 0..chunk {
                        let base = (done + i) * out_ch;
                        if let Some(slot) = out.get_mut(base + ml) {
                            *slot = mbuf_l[i];
                        }
                        if let Some(slot) = out.get_mut(base + mr) {
                            *slot = mbuf_r[i];
                        }
                        // Cue write: only when a deck is cued; the headphone
                        // channels were already zeroed for this frame in phase 1,
                        // and `out.get_mut` guards a missing pair on a 2ch device.
                        if any_cue {
                            if let Some((cl_ch, cr_ch)) = cue_ch {
                                if let Some(slot) = out.get_mut(base + cl_ch) {
                                    *slot = cbuf_l[i];
                                }
                                if let Some(slot) = out.get_mut(base + cr_ch) {
                                    *slot = cbuf_r[i];
                                }
                            }
                        }
                    }
                    done += chunk;
                }

                for ((deck, &pd), &dc) in
                    sh.decks.iter().zip(p.iter()).zip(declick.iter())
                {
                    deck.pos.store(pd.to_bits(), Ordering::Relaxed);
                    deck.declick.store(dc, Ordering::Relaxed);
                }
                // One epoch bump per deck that looped this block, so the video
                // worker re-seeks and the picture loops with the audio.
                for (deck, &w) in sh.decks.iter().zip(wrapped.iter()) {
                    if w {
                        deck.seek_epoch.fetch_add(1, Ordering::Relaxed);
                    }
                }
            },
            |e| eprintln!("output stream error: {e}"),
            None,
        )
        .context("build_output_stream")?;
    Ok(stream)
}

/// Owns the live audio streams and the shared transport state.
pub struct Engine {
    _in_stream: cpal::Stream,
    /// The output stream is REBUILT (not hot-swapped) to load/unload a master
    /// FX — the started CLAP processor is not freely `Send`, so it must be
    /// created on, and only used on, the audio thread that owns this stream.
    /// `Option` so `set_master_fx` can `take()` + drop the old stream (parking
    /// its ring consumers back in `shared`) before building the replacement.
    out_stream: Option<cpal::Stream>,
    /// Kept so the output stream can be rebuilt on `set_master_fx`.
    out_dev: cpal::Device,
    out_cfg: cpal::SupportedStreamConfig,
    shared: Arc<Shared>,
    /// Wait-free producer for master-FX param changes; the audio callback drains
    /// it. `None` when no FX is loaded. Rebuilt with each `set_master_fx`. Behind
    /// a `Mutex` only so `set_fx_param(&self, …)` (GUI thread) can push without
    /// `&mut`; the lock is never taken on the audio thread.
    fx_tx: Mutex<Option<rtrb::Producer<ParamSet>>>,
}

/// Cheap `Send + Sync` read handle to one deck's live transport, for off-thread
/// consumers (e.g. the video sync worker). `Engine` itself is `!Send` (it owns
/// the `cpal::Stream`s), so a worker thread takes a `Transport` instead. Clone
/// freely; all reads are wait-free atomic loads. Each handle is scoped to a
/// single deck so two videos can be paced independently.
#[derive(Clone)]
pub struct Transport {
    shared: Arc<Shared>,
    deck: usize,
}

impl Transport {
    /// Current audio playhead of this deck, in seconds. `pos` is the fractional
    /// frame (f64 bits); content time is unchanged by the playback rate, so the
    /// video paced to this follows variable-rate playback automatically.
    pub fn playhead_secs(&self) -> f64 {
        f64::from_bits(self.shared.decks[self.deck].pos.load(Ordering::Relaxed))
            / self.shared.device_sr as f64
    }
    pub fn is_playing(&self) -> bool {
        self.shared.decks[self.deck].playing.load(Ordering::Relaxed)
    }
    /// Monotonic counter; a change since the last read means this deck's playhead
    /// jumped discontinuously (user seek / track load / loop wrap) → re-seek the
    /// video.
    pub fn seek_epoch(&self) -> u64 {
        self.shared.decks[self.deck].seek_epoch.load(Ordering::Acquire)
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
        // Cue/PFL is optional: a 2ch device simply has no headphone pair. If a
        // cue index is configured but out of range for this device, drop it to
        // `None` so the per-sample write is a silent no-op (vs. a hard error
        // that would block running on a stereo-only output).
        let routing = {
            let mut r = routing;
            let oob = |c: Option<usize>| c.is_some_and(|i| i >= out_ch);
            if oob(r.cue_l) || oob(r.cue_r) {
                eprintln!(
                    "engine: cue channels {:?}/{:?} out of range for a {out_ch}ch device — cue disabled",
                    r.cue_l, r.cue_r
                );
                r.cue_l = None;
                r.cue_r = None;
            }
            r
        };
        if let Some((al, ar)) = aux {
            if al >= in_ch || ar >= in_ch {
                return Err(anyhow!("aux channels {al}/{ar} out of range for a {in_ch}ch input"));
            }
        }

        // Mic ring + optional aux ring: input callback (producers) -> output.
        let mic_ch = 0usize;
        let (mut prod, cons) = rtrb::RingBuffer::<f32>::new(device_sr as usize * 2);
        let (mut aux_prod, aux_cons) = rtrb::RingBuffer::<f32>::new(device_sr as usize * 2);

        let shared = Arc::new(Shared {
            decks: [Deck::new(), Deck::new()],
            crossfade: AtomicF32::new(0.0),
            mic_gain: AtomicF32::new(1.0),
            aux_gain: AtomicF32::new(1.0),
            cue_gain: AtomicF32::new(1.0),
            underruns: AtomicU64::new(0),
            device_sr,
            out_ch,
            routing,
            aux,
            // Park the consumers; build_output_stream takes them on first build.
            mic_cons: Mutex::new(Some(cons)),
            aux_cons: Mutex::new(Some(aux_cons)),
        });

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

        let out_stream =
            build_output_stream(&out_dev, &out_cfg, shared.clone(), None)?;

        in_stream.play().context("input stream play")?;
        out_stream.play().context("output stream play")?;

        Ok(Self {
            _in_stream: in_stream,
            out_stream: Some(out_stream),
            out_dev,
            out_cfg,
            shared,
            fx_tx: Mutex::new(None),
        })
    }

    /// Load a master-bus CLAP insert from a `.clap` path (or clear it with
    /// `None`). The ENTIRE master mix (decks + mic + aux) passes through this one
    /// effect before hitting the device. Rebuilds ONLY the output stream — the
    /// mic input stream + the rings + all transport state are untouched; a brief
    /// audio gap on (re)build is acceptable. Returns the effect's param list (for
    /// the GUI to build sliders), or `[]` when cleared.
    ///
    /// The CLAP processor is constructed here on the main thread and moved into
    /// the new output callback that owns it; it is never sent to another thread
    /// once started (it is not freely `Send`).
    pub fn set_master_fx(&mut self, path: Option<&Path>) -> Result<Vec<ParamInfo>> {
        // Tear down the current output stream FIRST so its RingLease drops and
        // parks the consumers back in `shared` for the rebuild below. cpal has no
        // stable "swap callback" API, so we replace the whole Stream.
        // Build the new FX (if any) BEFORE tearing the old stream down, so a load
        // error leaves the current output stream running untouched.
        let (fx, tx, params) = match path {
            Some(p) => {
                let (fx, tx, params) =
                    MasterFx::load(p, self.shared.device_sr, MAX_FX_BLOCK as u32)?;
                (Some(fx), Some(tx), params)
            }
            None => (None, None, Vec::new()),
        };

        // Drop the old output stream FIRST: its callback's RingLease drops and
        // parks the ring consumers back in `shared`, so the rebuild below can
        // `.take()` them. (cpal has no callback-swap API → replace the Stream.)
        // The mic INPUT stream + the rings + all transport state stay alive.
        self.out_stream = None;

        let new_stream =
            build_output_stream(&self.out_dev, &self.out_cfg, self.shared.clone(), fx)?;
        new_stream.play().context("output stream play (fx reload)")?;
        self.out_stream = Some(new_stream);
        *self.fx_tx.lock().unwrap() = tx;
        Ok(params)
    }

    /// Push a master-FX param change (wait-free on the audio side). The audio
    /// callback drains it on the next block. No-op if no FX is loaded. `value` is
    /// in the param's plain (un-normalized) range. Called from the GUI thread.
    pub fn set_fx_param(&self, id: u32, value: f64) {
        if let Ok(mut guard) = self.fx_tx.lock() {
            if let Some(tx) = guard.as_mut() {
                let _ = tx.push(ParamSet { id, value });
            }
        }
    }

    /// List installed `SuperDuper*.clap` bundles the user can pick as the master
    /// insert (sorted by file name).
    pub fn list_fx() -> Vec<PathBuf> {
        list_superduper_clap()
    }

    /// Decode a file (on the calling thread) and load it into `deck` at pos 0.
    /// Returns the track duration in seconds.
    pub fn load_track(&self, deck: usize, path: &Path) -> Result<f64> {
        let d = &self.shared.decks[deck_idx(deck)];
        let track = decode_track(path, self.shared.device_sr)?;
        let secs = track.duration_secs();
        d.pos.store(0.0f64.to_bits(), Ordering::Relaxed);
        d.seek_to.store(-1, Ordering::Relaxed);
        d.seek_epoch.fetch_add(1, Ordering::Release);
        d.track.store(Some(Arc::new(track)));
        Ok(secs)
    }

    /// Decode `path` on a background thread, then swap it into `deck`. Returns
    /// immediately; poll [`is_loading`](Self::is_loading) for completion. The mic
    /// keeps passing through while decoding. Bumps that deck's `seek_epoch` so
    /// its video re-seeks to the new content.
    pub fn load_track_background(&self, deck: usize, path: PathBuf) {
        let deck = deck_idx(deck);
        let shared = self.shared.clone();
        std::thread::spawn(move || {
            let d = &shared.decks[deck];
            d.loading.store(true, Ordering::Relaxed);
            match decode_track(&path, shared.device_sr) {
                Ok(track) => {
                    d.playing.store(false, Ordering::Relaxed);
                    d.pos.store(0.0f64.to_bits(), Ordering::Relaxed);
                    d.seek_to.store(-1, Ordering::Relaxed);
                    d.seek_epoch.fetch_add(1, Ordering::Release);
                    d.track.store(Some(Arc::new(track)));
                }
                Err(e) => eprintln!("load failed: {e:#}"),
            }
            d.loading.store(false, Ordering::Relaxed);
        });
    }

    pub fn is_loading(&self, deck: usize) -> bool {
        self.shared.decks[deck_idx(deck)].loading.load(Ordering::Relaxed)
    }

    pub fn play(&self, deck: usize) {
        self.shared.decks[deck_idx(deck)].playing.store(true, Ordering::Relaxed);
    }
    pub fn pause(&self, deck: usize) {
        self.shared.decks[deck_idx(deck)].playing.store(false, Ordering::Relaxed);
    }
    pub fn toggle(&self, deck: usize) -> bool {
        let d = &self.shared.decks[deck_idx(deck)];
        let next = !d.playing.load(Ordering::Relaxed);
        d.playing.store(next, Ordering::Relaxed);
        next
    }
    pub fn is_playing(&self, deck: usize) -> bool {
        self.shared.decks[deck_idx(deck)].playing.load(Ordering::Relaxed)
    }
    pub fn seek_secs(&self, deck: usize, secs: f64) {
        let d = &self.shared.decks[deck_idx(deck)];
        let frame = (secs.max(0.0) * self.shared.device_sr as f64) as i64;
        d.seek_to.store(frame, Ordering::Relaxed);
        d.seek_epoch.fetch_add(1, Ordering::Release);
    }
    pub fn set_deck_gain(&self, deck: usize, g: f32) {
        self.shared.decks[deck_idx(deck)].gain.store(g, Ordering::Relaxed);
    }
    /// Set `deck`'s playback rate (1.0 = original speed/pitch). >1 plays faster +
    /// higher (vinyl-style, NOT pitch-preserving), <1 slower + lower. Clamped to
    /// `0.25..=4.0` (two octaves either way) to keep interpolation sane and the
    /// playhead advance bounded. The ±8% beatmatch range lives well inside this.
    pub fn set_rate(&self, deck: usize, rate: f32) {
        let r = rate.clamp(MIN_RATE, MAX_RATE);
        self.shared.decks[deck_idx(deck)].rate.store(r, Ordering::Relaxed);
    }
    /// `deck`'s current playback rate (1.0 = original).
    pub fn rate(&self, deck: usize) -> f32 {
        self.shared.decks[deck_idx(deck)].rate.load(Ordering::Relaxed)
    }
    /// SYNC: match `deck` to the OTHER deck — BOTH tempo and beat PHASE. (1) set
    /// `deck`'s rate so its effective bpm (`track_bpm * rate`) equals the other's,
    /// then (2) seek `deck` so its beat phase lines up with the other's, so their
    /// beats coincide ("no clash"). No-op on tempo if either detected bpm is `<=0`;
    /// no-op on phase if either grid is unusable at the current playheads.
    pub fn sync_deck(&self, deck: usize, to: usize) {
        let me = deck_idx(deck);
        let other = deck_idx(to);
        // (1) Tempo: target the OTHER deck's *effective* tempo (grid bpm × rate).
        let target = self.bpm(other) * self.rate(other) as f64;
        let r = sync_rate(self.bpm(me), target);
        self.shared.decks[me].rate.store(r, Ordering::Relaxed);
        // (2) Phase: reproduce the other deck's beat phase on this deck, via a seek.
        if let (Some(t), Some(rt)) = (self.current_track(me), self.current_track(other)) {
            let (ph, rph) = (self.playhead_secs(me), self.playhead_secs(other));
            if let Some(target_secs) = crate::loops::beat_phase_align(&t.beats, ph, &rt.beats, rph)
            {
                self.seek_secs(me, target_secs);
            }
        }
    }
    /// Toggle headphone CUE / PFL (pre-fader listen) on `deck`. A cued deck's raw
    /// stereo (its deck gain, but pre-crossfade and pre-master-FX) is summed onto
    /// the cue bus and sent to the headphone channels — silent on a device with
    /// no cue pair (e.g. a 2ch output). The master path is unaffected.
    pub fn set_cue(&self, deck: usize, on: bool) {
        self.shared.decks[deck_idx(deck)].cue.store(on, Ordering::Relaxed);
    }
    pub fn is_cued(&self, deck: usize) -> bool {
        self.shared.decks[deck_idx(deck)].cue.load(Ordering::Relaxed)
    }
    /// Shared headphone CUE bus level (applied to all cued decks together).
    pub fn set_cue_gain(&self, g: f32) {
        self.shared.cue_gain.store(g, Ordering::Relaxed);
    }
    pub fn cue_gain(&self) -> f32 {
        self.shared.cue_gain.load(Ordering::Relaxed)
    }
    /// Engage a beat loop on `deck` over `[start_frame, end_frame)` (frame indices
    /// into the deck's resampled buffer, same units as the playhead). Ignored if
    /// `end <= start` (an empty/inverted region).
    pub fn set_loop(&self, deck: usize, start_frame: usize, end_frame: usize) {
        if end_frame <= start_frame {
            return;
        }
        let d = &self.shared.decks[deck_idx(deck)];
        d.loop_start.store(start_frame as i64, Ordering::Relaxed);
        d.loop_end.store(end_frame as i64, Ordering::Relaxed);
    }
    /// Disengage `deck`'s beat loop (playback continues past the old region).
    pub fn clear_loop(&self, deck: usize) {
        let d = &self.shared.decks[deck_idx(deck)];
        d.loop_start.store(-1, Ordering::Relaxed);
        d.loop_end.store(-1, Ordering::Relaxed);
    }
    /// `deck`'s active beat loop as `(start_frame, end_frame)`, or `None` if no
    /// valid loop is engaged. For the GUI to draw + highlight the region.
    pub fn loop_region(&self, deck: usize) -> Option<(usize, usize)> {
        let d = &self.shared.decks[deck_idx(deck)];
        let s = d.loop_start.load(Ordering::Relaxed);
        let e = d.loop_end.load(Ordering::Relaxed);
        (s >= 0 && e > s).then_some((s as usize, e as usize))
    }
    /// Crossfader position: `0.0` = full deck A, `1.0` = full deck B (equal-power).
    pub fn set_crossfade(&self, x: f32) {
        self.shared.crossfade.store(x.clamp(0.0, 1.0), Ordering::Relaxed);
    }
    pub fn crossfade(&self) -> f32 {
        self.shared.crossfade.load(Ordering::Relaxed)
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
    pub fn playhead_secs(&self, deck: usize) -> f64 {
        f64::from_bits(self.shared.decks[deck_idx(deck)].pos.load(Ordering::Relaxed))
            / self.shared.device_sr as f64
    }
    pub fn duration_secs(&self, deck: usize) -> f64 {
        self.shared.decks[deck_idx(deck)]
            .track
            .load()
            .as_ref()
            .map(|t| t.duration_secs())
            .unwrap_or(0.0)
    }
    /// Snapshot `deck`'s currently loaded track (cheap `Arc` clone) for the GUI
    /// to read `waveform`/`beats`/`downbeats`/`bpm`. `None` if nothing is loaded.
    pub fn current_track(&self, deck: usize) -> Option<Arc<TrackBuf>> {
        self.shared.decks[deck_idx(deck)].track.load_full()
    }
    /// Detected tempo of `deck`'s loaded track, or `0.0` if none.
    pub fn bpm(&self, deck: usize) -> f64 {
        self.shared.decks[deck_idx(deck)]
            .track
            .load()
            .as_ref()
            .map(|t| t.bpm)
            .unwrap_or(0.0)
    }
    pub fn device_sr(&self) -> u32 {
        self.shared.device_sr
    }
    /// A cloneable, thread-safe read handle to `deck`'s transport, for off-thread
    /// consumers (the video sync worker). Each deck has its own playhead/epoch.
    pub fn transport(&self, deck: usize) -> Transport {
        Transport { shared: self.shared.clone(), deck: deck_idx(deck) }
    }
    pub fn underruns(&self) -> u64 {
        self.shared.underruns.load(Ordering::Relaxed)
    }
}

/// Pure beatmatch helper: the playback rate that makes a track at `my_bpm` play
/// at `target_bpm` (`target / my`), clamped to `0.25..=4.0`. Returns `1.0` if
/// either bpm is `<= 0` (an un-analyzed track can't be matched). No side effects,
/// no atomics — shared between [`Engine::sync_deck`] and the `selftest-sync` CLI
/// so the sync math is verifiable headlessly.
pub fn sync_rate(my_bpm: f64, target_bpm: f64) -> f32 {
    if my_bpm <= 0.0 || target_bpm <= 0.0 {
        return 1.0;
    }
    ((target_bpm / my_bpm) as f32).clamp(MIN_RATE, MAX_RATE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_rate_speeds_up_slower_track() {
        // A 120-BPM track matched to a 128-BPM target speeds up by 128/120.
        let r = sync_rate(120.0, 128.0);
        assert!((r - (128.0 / 120.0)) < 1e-6 && (r - (128.0 / 120.0)) > -1e-6);
        assert!(r > 1.0, "matching to a faster target must speed up");
    }

    #[test]
    fn sync_rate_slows_down_faster_track() {
        // A 140-BPM track matched to a 120-BPM target slows to 120/140 < 1.
        let r = sync_rate(140.0, 120.0);
        assert!((r - (120.0 / 140.0_f32)).abs() < 1e-6);
        assert!(r < 1.0);
    }

    #[test]
    fn sync_rate_identity_when_equal() {
        assert!((sync_rate(124.0, 124.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn sync_rate_zero_or_negative_bpm_is_noop() {
        assert_eq!(sync_rate(0.0, 128.0), 1.0);
        assert_eq!(sync_rate(120.0, 0.0), 1.0);
        assert_eq!(sync_rate(-5.0, 128.0), 1.0);
        assert_eq!(sync_rate(120.0, -1.0), 1.0);
    }

    #[test]
    fn sync_rate_clamps_extremes() {
        // 60 → 600 would be 10×, clamped to MAX_RATE.
        assert_eq!(sync_rate(60.0, 600.0), MAX_RATE);
        // 600 → 60 would be 0.1×, clamped to MIN_RATE.
        assert_eq!(sync_rate(600.0, 60.0), MIN_RATE);
    }
}
