//! Headless self-test harness for the video deck — pace, seek, image, and
//! visualizer smoke tests plus the synthetic/scripted clocks that drive them.
//!
//! Kept as a child module of `video` so it can reach `video.rs`'s private
//! production items (`Decoder`, `ffprobe`, `fit_box`, `pack_rgba`, …) via the
//! ancestor-privacy rule, while keeping the production file focused on the
//! decode/render path. No GUI, no audio device: every check runs the real
//! workers against a synthetic playhead.

use super::*;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Headless visualizer self-test: drive a real [`Visualizer`] with a synthetic
/// playhead over a track's beatgrid for a moment and report that it renders frames
/// and that intensity peaks on a beat vs between beats. No GUI, no audio device.
pub fn selftest_viz(beats: &[f64], bpm: f64) -> String {
    if beats.len() < 2 {
        return "  viz : no beat grid".to_string();
    }
    // Pure intensity check: on a beat ≈ 1.0, a quarter-beat later clearly less.
    let period = if bpm > 0.0 { 60.0 / bpm } else { 0.5 };
    let on = beat_intensity(beats, beats[1], bpm);
    let between = beat_intensity(beats, beats[1] + period * 0.5, bpm);

    // Live worker check: run against a synthetic clock anchored at beats[1].
    let start = Instant::now();
    let clock = SyntheticClock { start, step_s: 0.0 };
    let viz = Visualizer::open(clock, beats.to_vec(), bpm);
    std::thread::sleep(Duration::from_millis(150));
    let gen = viz.slot().generation();
    let dims = viz.slot().with_latest(|f| (f.width, f.height)).unwrap_or((0, 0));
    drop(viz);

    let ok = on > 0.95 && between < on * 0.6 && gen > 0 && dims == (VIS_W, VIS_H);
    format!(
        "  viz : intensity on-beat {on:.2} vs mid-beat {between:.2}, \
rendered {gen} frames @ {}x{} — {}",
        dims.0,
        dims.1,
        if ok { "PASS" } else { "FAIL" }
    )
}

/// Headless smoke test of the video path (no GUI, no audio device): probe, read
/// a frame at t=0 and a few frames later (verify they differ → real motion), and
/// a seek read at mid-duration. Returns a human-readable report.
pub fn selftest_video(path: &Path) -> String {
    let pb = path.to_path_buf();
    let probe = match ffprobe(&pb) {
        Some(p) => p,
        None => return "  video : none (audio-only)".to_string(),
    };
    let (w, h) = fit_box(probe.width, probe.height, 480, 480);
    let fps = if probe.fps.is_finite() && probe.fps > 0.0 {
        probe.fps.clamp(1.0, 120.0)
    } else {
        25.0
    };
    let mut out = format!(
        "  video : {}x{} @ {:.2}fps  (scaled {w}x{h}, {} B/frame)",
        probe.width,
        probe.height,
        fps,
        (w as usize) * (h as usize) * 4
    );
    let t_late = 10.0 / fps;
    match (
        read_frame_luma(&pb, 0.0, w, h, fps),
        read_frame_luma(&pb, t_late, w, h, fps),
    ) {
        (Some(a), Some(b)) => {
            let verdict = if (a - b).abs() > 0.3 {
                "differ ✓ (real motion)"
            } else {
                "near-identical"
            };
            out.push_str(&format!(
                "\n  frames: t=0 luma={a:.1}, t={t_late:.2}s luma={b:.1}  → {verdict}"
            ));
        }
        _ => out.push_str("\n  frames: ERROR reading frames"),
    }
    if let Some(dur) = ffprobe_duration(&pb) {
        match read_frame_luma(&pb, dur / 2.0, w, h, fps) {
            Some(m) => out.push_str(&format!(
                "\n  seek  : t={:.1}s read OK (luma={m:.1}) ✓",
                dur / 2.0
            )),
            None => out.push_str("\n  seek  : ERROR reading frame after seek"),
        }
    }
    out
}

/// Headless smoke test of the static-image source: open `path` as an [`ImageStill`]
/// and report its scaled dimensions, generation, and whether the slot holds a
/// frame — the autonomous check that an image deck publishes exactly one frame.
pub fn selftest_image(path: &Path) -> String {
    match ImageStill::open(path) {
        Some(still) => {
            let slot = still.slot();
            let gen = slot.generation();
            let dims = slot
                .with_latest(|fr| (fr.width, fr.height))
                .unwrap_or((0, 0));
            let has_frame = slot.with_latest(|_| true).unwrap_or(false);
            format!(
                "image OK: {}x{}, generation={gen}, slot has frame={has_frame}",
                dims.0, dims.1
            )
        }
        None => "image FAILED".to_string(),
    }
}

/// In-process decode of a single RGBA frame at time `t`, returning its mean luma
/// (0..255), or `None` on failure. (`fps` is unused now that PTS drives timing,
/// but kept in the signature so the selftest call sites stay unchanged.)
fn read_frame_luma(path: &Path, t: f64, w: u32, h: u32, _fps: f64) -> Option<f64> {
    ensure_ffmpeg_init();
    let mut dec = Decoder::open(path, w, h)?;
    if t > 0.0 {
        dec.seek(t);
    }
    let (_pts, frame) = dec.next_frame()?;
    let buf = pack_rgba(frame, w, h);
    let px = (w as usize) * (h as usize);
    let mut sum = 0.0f64;
    for p in buf.chunks_exact(4) {
        sum += 0.30 * p[0] as f64 + 0.59 * p[1] as f64 + 0.11 * p[2] as f64;
    }
    Some(sum / px as f64)
}

/// A wall-clock playhead for the headless pace test. `step_s > 0` quantizes the
/// reported time into steps, simulating a coarse audio-callback playhead (which
/// only advances once per output buffer); `0` is perfectly smooth. Never seeks.
#[derive(Clone, Copy)]
struct SyntheticClock {
    start: Instant,
    step_s: f64,
}

impl VideoClock for SyntheticClock {
    fn pos_secs(&self) -> f64 {
        let e = self.start.elapsed().as_secs_f64();
        if self.step_s > 0.0 {
            (e / self.step_s).floor() * self.step_s
        } else {
            e
        }
    }
    fn epoch(&self) -> u64 {
        0
    }
}

/// Headless A/V pacing measurement: run the real video worker against a synthetic
/// playhead and report cadence (smoothness) + sync stats. Runs twice — a smooth
/// playhead and a 43 ms-stepped one (a coarse audio-callback clock) — to see if a
/// coarse playhead is what causes the judder. No GUI, no audio device.
pub fn pace_test(path: &Path) -> String {
    let smooth = measure_pace(path, 0.0);
    let stepped = measure_pace(path, 0.043);
    format!(
        "{smooth}\n  --- same, but a 43ms-STEPPED (coarse audio-callback) playhead ---\n{stepped}"
    )
}

fn measure_pace(path: &Path, step_s: f64) -> String {
    const DUR_S: f64 = 8.0;
    let start = Instant::now();
    let deck = match VideoDeck::open(path.to_path_buf(), SyntheticClock { start, step_s }) {
        Some(d) => d,
        None => return "  pace : no video stream".to_string(),
    };

    // Record (present_time, content_pts) for each newly presented frame.
    let mut presents: Vec<(f64, f64)> = Vec::new();
    let mut last_gen = 0u64;
    while start.elapsed().as_secs_f64() < DUR_S {
        let gen = deck.generation();
        if gen != last_gen {
            last_gen = gen;
            presents.push((start.elapsed().as_secs_f64(), deck.frame_pts()));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let offset = deck.offset_ms();
    drop(deck); // stop the worker / free the decode context

    if presents.len() < 3 {
        return format!("  pace : only {} frames in {DUR_S:.0}s — broken", presents.len());
    }

    // Cadence: inter-present gaps (smoothness).
    let mut gaps: Vec<f64> = presents.windows(2).map(|w| w[1].0 - w[0].0).collect();
    gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = gaps.len();
    let mean_gap = gaps.iter().sum::<f64>() / n as f64;
    let median = gaps[n / 2];
    let p95 = gaps[((n as f64 * 0.95) as usize).min(n - 1)];
    let max_gap = *gaps.last().unwrap();
    let freezes = gaps.iter().filter(|&&g| g > 2.5 * median).count();

    // Sync: present_time − content_pts. With offset comp the worker shows each
    // frame ~`offset` early, so this is ≈ −offset; stddev is the pacing wobble.
    let errs: Vec<f64> = presents.iter().map(|&(t, p)| (t - p) * 1000.0).collect();
    let mean_err = errs.iter().sum::<f64>() / errs.len() as f64;
    let std_err =
        (errs.iter().map(|e| (e - mean_err).powi(2)).sum::<f64>() / errs.len() as f64).sqrt();

    format!(
        "  pace : {} frames in {DUR_S:.0}s (~{:.0} fps presented)\n  \
cadence: median {:.0}ms, mean {:.0}ms, p95 {:.0}ms, max {:.0}ms, freezes(>2.5×) {}\n  \
sync   : mean {:+.0}ms, stddev {:.0}ms  (display-comp offset {offset}ms)",
        presents.len(),
        presents.len() as f64 / DUR_S,
        median * 1000.0,
        mean_gap * 1000.0,
        p95 * 1000.0,
        max_gap * 1000.0,
        freezes,
        mean_err,
        std_err,
    )
}

/// A scriptable playhead for the seek-stress test: advances in real time and can
/// be jumped (forward or backward) by the test thread, bumping the seek epoch.
#[derive(Clone)]
struct ScriptedClock {
    base: Arc<Mutex<(Instant, f64)>>, // (anchor instant, playhead at anchor)
    epoch: Arc<AtomicU64>,
}

impl ScriptedClock {
    fn new(pos: f64) -> Self {
        Self {
            base: Arc::new(Mutex::new((Instant::now(), pos))),
            epoch: Arc::new(AtomicU64::new(0)),
        }
    }
    fn seek(&self, pos: f64) {
        if let Ok(mut g) = self.base.lock() {
            *g = (Instant::now(), pos);
        }
        self.epoch.fetch_add(1, Ordering::Release);
    }
}

impl VideoClock for ScriptedClock {
    fn pos_secs(&self) -> f64 {
        match self.base.lock() {
            Ok(g) => g.1 + g.0.elapsed().as_secs_f64(),
            Err(_) => 0.0,
        }
    }
    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }
}

/// Headless seek-stress test: script a sequence of forward and backward seeks and
/// measure how long each takes to produce a fresh frame (recovery latency), so a
/// backward-seek bug or stall shows up as a long/"STUCK" entry. No GUI/audio.
pub fn seek_test(path: &Path) -> String {
    let dur = ffprobe_duration(&path.to_path_buf()).unwrap_or(20.0).max(2.0);
    let clock = ScriptedClock::new(dur * 0.5);
    let deck = match VideoDeck::open(path.to_path_buf(), clock.clone()) {
        Some(d) => d,
        None => return "  seek : no video stream".to_string(),
    };
    // Wait for the first frame so the deck is warm.
    let warm = Instant::now();
    while deck.generation() == 0 && warm.elapsed().as_secs_f64() < 3.0 {
        std::thread::sleep(Duration::from_millis(2));
    }

    // forward, backward, far-forward, to-start, mid, to-start again, far-back…
    let targets = [
        dur * 0.7,
        dur * 0.2,
        dur * 0.9,
        0.0,
        dur * 0.4,
        0.0,
        dur * 0.95,
        dur * 0.1,
    ];
    let mut out = String::from("  seek : recovery (seek → next fresh frame):");
    let mut prev = dur * 0.5;
    for &t in &targets {
        let pre_gen = deck.generation();
        clock.seek(t);
        let t0 = Instant::now();
        let mut recov = None;
        while t0.elapsed().as_secs_f64() < 2.5 {
            // +2: skip a possibly in-flight pre-reseek frame.
            if deck.generation() >= pre_gen + 2 {
                recov = Some(t0.elapsed().as_secs_f64() * 1000.0);
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let dir = if t < prev { "back" } else { "fwd " };
        prev = t;
        std::thread::sleep(Duration::from_millis(250)); // let it play a beat
        out.push_str(&format!(
            "\n    {dir} →{t:6.1}s : {}",
            match recov {
                Some(ms) => format!("{ms:.0}ms"),
                None => "STUCK (no frame in 2.5s)".into(),
            }
        ));
    }
    drop(deck);
    out
}

/// `ffprobe` the container duration in seconds.
fn ffprobe_duration(path: &PathBuf) -> Option<f64> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}
