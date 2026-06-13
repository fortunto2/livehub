//! M3 video deck — ONE video deck whose displayed frame = f(audio playhead).
//!
//! The loaded media file's AUDIO already drives the engine playhead (symphonia
//! decodes the mp4/mov AAC stream via `load_track_background`). Here a worker
//! thread decodes that same file's VIDEO frames IN-PROCESS via `ffmpeg-next`
//! (libav* 8) and PACES them to the audio playhead via a [`Transport`] handle,
//! so play/seek of the audio make the video follow. Frame = f(playhead).
//!
//! Decode is in-process (no `ffmpeg` CLI child, no rawvideo pipe): the worker
//! owns an open `ffmpeg::format::Input`, a `decoder`, and a swscale `scaler` to
//! RGBA at the fit-box size. A transport discontinuity (seek / load) is served
//! by `ictx.seek(...)` + `decoder.flush()` on the ALREADY-OPEN container — a
//! frame-accurate, near-instant seek with no process respawn (the win over the
//! old CLI pipe, where a backward seek cost ~50-90ms to re-launch ffmpeg).
//!
//! `ffprobe` (CLI) is still used for the lightweight dims/fps/duration probe.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use ffmpeg_next as ffmpeg;
use ffmpeg::format::Pixel;

use livehub_engine::Transport;

// Headless self-test harness (pace/seek/image/viz + synthetic clocks) lives in
// its own file to keep this one focused on the production decode/render path.
#[path = "video_selftest.rs"]
mod selftest;
pub use selftest::{pace_test, seek_test, selftest_image, selftest_video, selftest_viz};

/// Only a genuine large stall — not the normal seek latency, which the in-process
/// `ictx.seek` now serves near-instantly — should trigger a hard re-seek. Set well
/// above typical jitter to avoid a re-seek storm.
const MAX_DRIFT_SECS: f64 = 2.0;

/// Drop (skip presenting) and pull the next frame only when the video is more
/// than this far behind the compensated playhead — generous enough that normal
/// jitter doesn't cause periodic drop-freezes ("залип").
const DROP_TOL_SECS: f64 = 0.10;

/// Default display-pipeline compensation (ms): the video frame for content-time
/// `t` is shown slightly before the audio reaches `t`, so that after the GUI →
/// texture → GPU → display delay it lands in sync. User-tunable at runtime.
const DEFAULT_OFFSET_MS: i64 = 80;

/// Cap on wall-clock extrapolation when smoothing the stepped audio playhead into
/// a continuous one — must exceed the audio buffer step, and bounds how far the
/// estimate runs ahead while paused.
const SMOOTH_MAX_EXTRAP: f64 = 0.10;

/// One-time `ffmpeg::init()` for the whole process (registers codecs / formats).
fn ensure_ffmpeg_init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if let Err(e) = ffmpeg::init() {
            eprintln!("video: DISABLED — ffmpeg init failed: {e}");
        }
    });
}

/// A monotonic playhead source for the video worker, so it can run against the
/// real audio engine (`Transport`) or a synthetic clock in the headless pace test.
pub trait VideoClock: Send + 'static {
    fn pos_secs(&self) -> f64;
    fn epoch(&self) -> u64;
}

impl VideoClock for Transport {
    fn pos_secs(&self) -> f64 {
        self.playhead_secs()
    }
    fn epoch(&self) -> u64 {
        self.seek_epoch()
    }
}

/// One decoded RGBA video frame, ready for upload to a GPU texture.
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Lock-light shared slot the worker writes and the GUI reads. The GUI only
/// re-uploads to the GPU when `generation` changes, so it never re-reads an
/// unchanged frame. Public so `VideoSource` can hand the same slot handle out for
/// every source kind (file worker / camera worker / static image).
///
/// The audio RT callback NEVER touches this slot — only the video worker thread
/// writes it and the GUI thread reads it — so its plain `Mutex` is safe and does
/// not contradict the no-locks-in-callback rule (see crate docs in lib.rs).
#[derive(Default)]
pub struct FrameSlot {
    frame: Mutex<Option<VideoFrame>>,
    generation: AtomicU64,
    /// Content time (seconds, f64 bits) of the latest presented frame — lets the
    /// headless pace test measure sync error against the playhead.
    pts_bits: AtomicU64,
}

impl FrameSlot {
    /// The latest frame generation (Acquire pairs with the worker's Release bump).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Content time (seconds) of the latest presented frame.
    pub fn frame_pts(&self) -> f64 {
        f64::from_bits(self.pts_bits.load(Ordering::Acquire))
    }

    /// Run `f` over the latest frame, if any — lock held only for the closure.
    pub fn with_latest<R>(&self, f: impl FnOnce(&VideoFrame) -> R) -> Option<R> {
        let guard = self.frame.lock().ok()?;
        guard.as_ref().map(f)
    }
}

/// A live video deck: an in-process ffmpeg-driven worker thread plus the shared
/// frame slot it feeds. Dropping the deck signals the worker to stop and joins
/// it; the worker owns the decode context, which drops cleanly (RAII).
pub struct VideoDeck {
    slot: Arc<FrameSlot>,
    running: Arc<AtomicBool>,
    /// Display-pipeline compensation in ms (present this much before content-time).
    /// Shared with the worker; the GUI exposes it as a slider.
    offset_ms: Arc<AtomicI64>,
    worker: Option<std::thread::JoinHandle<()>>,
}

/// Video stream metadata pulled from `ffprobe`.
struct Probe {
    width: u32,
    height: u32,
    fps: f64,
}

impl VideoDeck {
    /// Probe `path` for a video stream and, if present, spawn the sync worker.
    /// Returns `None` when the file has no video stream (audio-only) or ffprobe
    /// fails — the caller then runs audio-only with no picture.
    pub fn open<C: VideoClock>(path: PathBuf, clock: C) -> Option<VideoDeck> {
        let probe = match ffprobe(&path) {
            Some(p) => p,
            None => {
                eprintln!("video: no video stream in {} (audio-only)", path.display());
                return None;
            }
        };

        // Decode at a modest size (fit within a 480×480 box, only downscale, even
        // dims) so each RGBA frame stays light and the worker sustains real time;
        // the GUI shows it ≤300px tall anyway.
        let (width, height) = fit_box(probe.width, probe.height, 480, 480);
        // Clamp the paced fps to something sane; ffprobe can report 0/0.
        let fps = if probe.fps.is_finite() && probe.fps > 0.0 {
            probe.fps.clamp(1.0, 120.0)
        } else {
            25.0
        };

        let slot = Arc::new(FrameSlot::default());
        let running = Arc::new(AtomicBool::new(true));
        let offset_ms = Arc::new(AtomicI64::new(DEFAULT_OFFSET_MS));

        let worker = {
            let slot = slot.clone();
            let running = running.clone();
            let offset_ms = offset_ms.clone();
            std::thread::Builder::new()
                .name("video-worker".into())
                .spawn(move || {
                    worker_loop(path, clock, slot, running, offset_ms, width, height, fps);
                })
                .ok()?
        };

        Some(VideoDeck { slot, running, offset_ms, worker: Some(worker) })
    }

    /// The shared frame slot the worker feeds. Exposed so `VideoSource` can read
    /// every source kind (file / camera / image) through one uniform slot handle.
    pub fn slot(&self) -> &Arc<FrameSlot> {
        &self.slot
    }

    /// The worker's monotonic frame generation (delegates to the slot). The
    /// headless pace/seek tests compare it to spot newly presented frames.
    pub fn generation(&self) -> u64 {
        self.slot.generation()
    }

    /// Content time (seconds) of the latest presented frame (delegates to slot).
    pub fn frame_pts(&self) -> f64 {
        self.slot.frame_pts()
    }

    /// Display-pipeline compensation in ms (positive = show the video earlier).
    pub fn offset_ms(&self) -> i64 {
        self.offset_ms.load(Ordering::Relaxed)
    }

    pub fn set_offset_ms(&self, ms: i64) {
        self.offset_ms.store(ms, Ordering::Relaxed);
    }
}

impl Drop for VideoDeck {
    fn drop(&mut self) {
        // The worker checks `running` each iteration and never blocks on I/O for
        // more than one packet read (no child process to park on), so flipping
        // the flag and joining is a clean, prompt shutdown; the worker owns the
        // ffmpeg context and drops it on exit (RAII).
        self.running.store(false, Ordering::Release);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

// ===========================================================================
// Live camera source (M5b-1).
// ===========================================================================

/// Camera capture box: the live webcam is decoded to this fixed size and the GUI
/// upscales it to the composite. 480×270 = 16:9, light enough to copy each frame.
const CAM_W: u32 = 480;
const CAM_H: u32 = 270;

/// A live webcam deck. Unlike [`VideoDeck`] there is no playhead to pace against —
/// a worker thread reads raw RGBA frames straight off an `ffmpeg` avfoundation
/// capture child and publishes each into a [`FrameSlot`] the moment it arrives,
/// using the SAME lock + pts_bits.store + generation.fetch_add sequence the file
/// worker uses, so the GUI consumes camera frames byte-for-byte identically.
pub struct CameraDeck {
    slot: Arc<FrameSlot>,
    running: Arc<AtomicBool>,
    /// The `ffmpeg` capture child; killed on drop so the camera LED goes out.
    child: Option<std::process::Child>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl CameraDeck {
    /// Enumerate avfoundation video capture devices as `"[idx] Name"` strings, in
    /// index order. `ffmpeg -list_devices` exits non-zero and prints the list to
    /// STDERR; we parse the lines under the "AVFoundation video devices" header and
    /// stop at the "AVFoundation audio devices" header. Returns `[]` if ffmpeg is
    /// missing or nothing parses.
    pub fn list_cameras() -> Vec<String> {
        let out = match Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-f",
                "avfoundation",
                "-list_devices",
                "true",
                "-i",
                "",
            ])
            .stdout(Stdio::null())
            .output()
        {
            Ok(o) => o,
            Err(_) => return Vec::new(), // ffmpeg not on PATH.
        };
        let text = String::from_utf8_lossy(&out.stderr);
        let mut cams = Vec::new();
        let mut in_video = false;
        for line in text.lines() {
            if line.contains("AVFoundation video devices") {
                in_video = true;
                continue;
            }
            if line.contains("AVFoundation audio devices") {
                break; // audio section starts → done with video devices.
            }
            if !in_video {
                continue;
            }
            // Each device line looks like:
            //   [AVFoundation indev @ 0x..] [0] FaceTime HD Camera
            // Drop the `[... indev ...]` prefix, then keep `[idx] Name`.
            if let Some((_, rest)) = line.split_once(']') {
                let rest = rest.trim();
                if rest.starts_with('[') {
                    cams.push(rest.to_string());
                }
            }
        }
        cams
    }

    /// Spawn an `ffmpeg` avfoundation capture for camera `index`, scaling to
    /// `CAM_W`×`CAM_H` RGBA on a rawvideo pipe, and start a worker that reads one
    /// full frame at a time and publishes it. Returns `None` only if the child
    /// fails to spawn; if the child spawns but produces nothing (no camera / denied
    /// permission) the deck is still `Some` and its slot simply stays empty → black.
    pub fn open(index: usize) -> Option<CameraDeck> {
        let mut child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "avfoundation",
                "-framerate",
                "30",
                "-video_size",
                "1280x720",
                "-i",
                &format!("{index}:none"),
                "-vf",
                &format!("scale={CAM_W}:{CAM_H}"),
                "-pix_fmt",
                "rgba",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let stdout = child.stdout.take()?;
        let slot = Arc::new(FrameSlot::default());
        let running = Arc::new(AtomicBool::new(true));

        let worker = {
            let slot = slot.clone();
            let running = running.clone();
            std::thread::Builder::new()
                .name("camera-worker".into())
                .spawn(move || camera_loop(stdout, slot, running))
                .ok()?
        };

        Some(CameraDeck { slot, running, child: Some(child), worker: Some(worker) })
    }

    /// The shared frame slot the capture worker feeds.
    pub fn slot(&self) -> &Arc<FrameSlot> {
        &self.slot
    }
}

impl Drop for CameraDeck {
    fn drop(&mut self) {
        // Signal the worker to stop, kill the ffmpeg child (releases the camera so
        // its LED goes out and the read returns EOF), then join the worker.
        self.running.store(false, Ordering::Release);
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// Camera worker: read exactly `CAM_W*CAM_H*4` bytes per frame off the ffmpeg
/// rawvideo pipe into a reused buffer and publish each one. PTS is irrelevant for
/// a live feed, so it rides a frame counter (seconds-ish) just to fill the slot's
/// `pts_bits`; the GUI only watches `generation`.
fn camera_loop(mut stdout: std::process::ChildStdout, slot: Arc<FrameSlot>, running: Arc<AtomicBool>) {
    use std::io::Read;
    let frame_bytes = (CAM_W as usize) * (CAM_H as usize) * 4;
    let mut buf = vec![0u8; frame_bytes];
    let mut frame_no: u64 = 0;
    while running.load(Ordering::Acquire) {
        // `read_exact` fills the whole frame or fails (EOF when the child dies).
        match stdout.read_exact(&mut buf) {
            Ok(()) => {
                present_frame(&slot, &buf, CAM_W, CAM_H, frame_no as f64 / 30.0);
                frame_no += 1;
            }
            Err(_) => break, // child exited / pipe closed → stop publishing.
        }
    }
}

// ===========================================================================
// Static image source (M5b-1).
// ===========================================================================

/// A single still image published once into a [`FrameSlot`]. No worker, no decode
/// thread: `open` decodes + scales the image, publishes ONE frame (generation → 1
/// so the GUI uploads it), and that's the whole lifetime.
pub struct ImageStill {
    slot: Arc<FrameSlot>,
}

impl ImageStill {
    /// Decode `path`, fit it into a 480×480 box (downscale only, even dims), and
    /// publish the single RGBA frame. Returns `None` if the image fails to decode.
    pub fn open(path: &Path) -> Option<ImageStill> {
        let img = image::open(path).ok()?.to_rgba8();
        let (sw, sh) = fit_box(img.width(), img.height(), 480, 480);
        // `fit_box` only downscales; if the image already fits, `resize` to the
        // same dims is a cheap copy. Triangle filter = good quality, fast.
        let scaled = image::imageops::resize(&img, sw, sh, image::imageops::FilterType::Triangle);
        let (w, h) = (scaled.width(), scaled.height());
        let rgba = scaled.into_raw(); // tight w*h*4, no stride padding.

        let slot = Arc::new(FrameSlot::default());
        present_frame(&slot, &rgba, w, h, 0.0); // generation: 0 → 1.
        Some(ImageStill { slot })
    }

    /// The shared frame slot holding the single still.
    pub fn slot(&self) -> &Arc<FrameSlot> {
        &self.slot
    }
}

// ===========================================================================
// Beat-reactive visualizer source (M5b-2): no input media — a worker renders a
// generated picture that PULSES on the deck's beatgrid, driven by the deck's own
// playhead (a VideoClock) so it stays locked to the audio. For music-only decks.
// ===========================================================================

const VIS_W: u32 = 480;
const VIS_H: u32 = 270;

/// Beat-reactive pulse intensity in `0..=1` at content time `t`: `1.0` right on
/// the most recent beat, decaying exponentially toward the next (window scaled by
/// `bpm`). Returns `0` before the first beat or with no grid. Pure + tested so the
/// "react to the beat" behaviour is verified without rendering a pixel.
pub fn beat_intensity(beats: &[f64], t: f64, bpm: f64) -> f32 {
    if beats.is_empty() || t < beats[0] {
        return 0.0;
    }
    let i = beats.partition_point(|&b| b <= t).saturating_sub(1);
    let dt = t - beats[i];
    let period = if bpm > 0.0 { 60.0 / bpm } else { 0.5 };
    let tau = (period * 0.25).clamp(0.05, 0.25);
    (-dt / tau).exp() as f32
}

/// A generated, beat-locked picture. A worker renders ~30 fps into a [`FrameSlot`]
/// using the deck's playhead (any [`VideoClock`]) + a snapshot of its beatgrid, so
/// seeks/loops are reflected automatically (it just re-reads the playhead). Drop
/// stops + joins the worker.
pub struct Visualizer {
    slot: Arc<FrameSlot>,
    running: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Visualizer {
    /// Start a visualizer paced by `clock`, reacting to `beats` (seconds) at `bpm`.
    pub fn open<C: VideoClock>(clock: C, beats: Vec<f64>, bpm: f64) -> Visualizer {
        let slot = Arc::new(FrameSlot::default());
        let running = Arc::new(AtomicBool::new(true));
        let worker = {
            let slot = slot.clone();
            let running = running.clone();
            std::thread::Builder::new()
                .name("visualizer-worker".into())
                .spawn(move || visualizer_loop(clock, beats, bpm, slot, running))
                .ok()
        };
        Visualizer { slot, running, worker }
    }

    /// The shared frame slot the render worker feeds.
    pub fn slot(&self) -> &Arc<FrameSlot> {
        &self.slot
    }
}

impl Drop for Visualizer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// Render loop: ~30 fps, reading the playhead each frame and publishing a pulse.
fn visualizer_loop<C: VideoClock>(
    clock: C,
    beats: Vec<f64>,
    bpm: f64,
    slot: Arc<FrameSlot>,
    running: Arc<AtomicBool>,
) {
    let mut buf = vec![0u8; (VIS_W as usize) * (VIS_H as usize) * 4];
    while running.load(Ordering::Acquire) {
        let t = clock.pos_secs();
        let intensity = beat_intensity(&beats, t, bpm);
        render_visualizer(&mut buf, t, intensity, &beats);
        present_frame(&slot, &buf, VIS_W, VIS_H, t);
        std::thread::sleep(Duration::from_millis(33));
    }
}

/// Paint one visualizer frame: a dark vertical-gradient background lifted by the
/// beat `intensity`, a soft-edged centre disc whose radius tracks `intensity`, and
/// a hue that advances every beat so each beat flashes a fresh colour.
fn render_visualizer(buf: &mut [u8], t: f64, intensity: f32, beats: &[f64]) {
    let (w, h) = (VIS_W as i32, VIS_H as i32);
    let beat_idx = beats.partition_point(|&b| b <= t);
    let hue = (beat_idx as f32 * 47.0) % 360.0;
    let (dr, dg, db) = hsv_to_rgb(hue, 0.7, 0.55 + 0.45 * intensity);
    let (cx, cy) = (w / 2, h / 2);
    let r = 28.0 + 100.0 * intensity;
    let r2 = r * r;
    for y in 0..h {
        // Background: dim vertical gradient, brightened by the pulse.
        let bg = (8.0 + 18.0 * (y as f32 / h as f32) + 26.0 * intensity) as u8;
        for x in 0..w {
            let idx = ((y * w + x) as usize) * 4;
            let (mut pr, mut pg, mut pb) = (bg / 3, bg / 3, bg);
            let (dx, dy) = ((x - cx) as f32, (y - cy) as f32);
            let d2 = dx * dx + dy * dy;
            if d2 < r2 {
                let edge = 1.0 - d2 / r2; // bright centre → soft rim
                pr = lerp_u8(pr, dr, edge);
                pg = lerp_u8(pg, dg, edge);
                pb = lerp_u8(pb, db, edge);
            }
            buf[idx] = pr;
            buf[idx + 1] = pg;
            buf[idx + 2] = pb;
            buf[idx + 3] = 255;
        }
    }
}

fn lerp_u8(a: u8, b: u8, f: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * f.clamp(0.0, 1.0)) as u8
}

/// Minimal HSV→RGB (h in degrees, s/v in `0..=1`).
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let hp = (h.rem_euclid(360.0)) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    ((((r + m) * 255.0) as u8), (((g + m) * 255.0) as u8), (((b + m) * 255.0) as u8))
}

// ===========================================================================
// Unified video source (M5b-1): file deck, live camera, or static image — all
// publish RGBA into a `FrameSlot`, so the GUI's texture-upload + composite code
// reads every kind through one enum.
// ===========================================================================

/// One deck's picture: a paced file deck, a live webcam, or a static image. Each
/// variant feeds a [`FrameSlot`] with the identical publish sequence, so the GUI
/// reads them all uniformly via [`VideoSource::slot`].
pub enum VideoSource {
    File(VideoDeck),
    Camera(CameraDeck),
    Image(ImageStill),
    Visualizer(Visualizer),
}

impl VideoSource {
    /// The frame slot to read (generation + latest frame), whatever the kind.
    pub fn slot(&self) -> &Arc<FrameSlot> {
        match self {
            VideoSource::File(d) => d.slot(),
            VideoSource::Camera(c) => c.slot(),
            VideoSource::Image(i) => i.slot(),
            VideoSource::Visualizer(v) => v.slot(),
        }
    }

    /// Display-pipeline compensation (ms). Only a paced file deck has one; camera
    /// and image have no playhead to compensate against → `0`.
    pub fn offset_ms(&self) -> i32 {
        match self {
            VideoSource::File(d) => d.offset_ms() as i32,
            _ => 0,
        }
    }

    /// Set the file deck's display-comp offset; no-op for camera / image.
    pub fn set_offset_ms(&self, ms: i32) {
        if let VideoSource::File(d) = self {
            d.set_offset_ms(ms as i64);
        }
    }

    /// True only for the paced file deck (the only kind with a meaningful sync
    /// offset — the GUI gates its "video sync" slider on this).
    pub fn is_file(&self) -> bool {
        matches!(self, VideoSource::File(_))
    }

    /// Short kind tag for the GUI's deck-title label.
    pub fn kind(&self) -> &'static str {
        match self {
            VideoSource::File(_) => "file",
            VideoSource::Camera(_) => "cam",
            VideoSource::Image(_) => "image",
            VideoSource::Visualizer(_) => "viz",
        }
    }
}

/// In-process video decoder: an open container + decoder + RGBA scaler, with
/// frame-accurate `seek` on the ALREADY-OPEN container and a one-packet-at-a-time
/// `next_frame` pull (so seeks can interleave between reads without a respawn).
struct Decoder {
    ictx: ffmpeg::format::context::Input,
    decoder: ffmpeg::decoder::Video,
    scaler: ffmpeg::software::scaling::Context,
    stream_index: usize,
    /// Seconds per stream time-base tick — converts a frame's raw PTS to seconds.
    tb_f64: f64,
    /// Reusable decode + scale frames, allocated once.
    raw: ffmpeg::frame::Video,
    rgba: ffmpeg::frame::Video,
    /// Set once `next_frame` has drained all packets and the decoder is empty.
    eof: bool,
    /// Frames with PTS strictly below this are decoded but skipped (the keyframe
    /// lead-in after a fast-seek), so the first PRESENTED frame is at/after the
    /// seek target — reproducing the old CLI `-ss` fast-seek trim.
    min_pts: f64,
}

impl Decoder {
    /// Open `path`, select the best video stream, and build a decoder + a swscale
    /// scaler to RGBA at `(out_w, out_h)`. Returns `None` on any failure.
    fn open(path: &Path, out_w: u32, out_h: u32) -> Option<Decoder> {
        let ictx = ffmpeg::format::input(&path).ok()?;

        let stream = ictx.streams().best(ffmpeg::media::Type::Video)?;
        let stream_index = stream.index();
        let time_base = stream.time_base();
        let tb_f64 = f64::from(time_base.0) / f64::from(time_base.1);

        let mut codec_ctx =
            ffmpeg::codec::context::Context::from_parameters(stream.parameters()).ok()?;
        codec_ctx.set_threading(ffmpeg::threading::Config {
            kind: ffmpeg::threading::Type::Frame,
            count: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
        });
        let decoder = codec_ctx.decoder().video().ok()?;

        let scaler = ffmpeg::software::scaling::Context::get(
            decoder.format(),
            decoder.width(),
            decoder.height(),
            Pixel::RGBA,
            out_w,
            out_h,
            ffmpeg::software::scaling::Flags::FAST_BILINEAR,
        )
        .ok()?;

        Some(Decoder {
            ictx,
            decoder,
            scaler,
            stream_index,
            tb_f64,
            raw: ffmpeg::frame::Video::empty(),
            rgba: ffmpeg::frame::Video::empty(),
            eof: false,
            min_pts: -f64::INFINITY,
        })
    }

    /// Frame-accurate fast-seek to `t` seconds on the OPEN container: jump to the
    /// keyframe at/before `t`, flush the decoder's stale reference frames, and arm
    /// the post-seek PTS trim so the next presented frame is at/after `t`. This is
    /// the near-instant path that replaces the old kill+respawn.
    fn seek(&mut self, t: f64) {
        // `ictx.seek` units are AV_TIME_BASE (microseconds), NOT stream time-base.
        // `..seek_us` = "land on a keyframe at/before seek_us". Seek a touch before
        // `t` so the wanted frame is reached after the keyframe lead-in.
        let seek_us = ((t - 0.5).max(0.0) * 1_000_000.0) as i64;
        let _ = self.ictx.seek(seek_us, ..seek_us);
        self.decoder.flush();
        self.eof = false;
        self.min_pts = t;
    }

    /// Pull, decode, and scale ONE RGBA frame at/after the current position.
    /// Returns `(pts_secs, &rgba_frame)`, or `None` at end-of-stream. Reads packets
    /// one at a time so a `seek` can interleave between calls. Skips frames whose
    /// PTS is below `min_pts` (the fast-seek keyframe lead-in).
    fn next_frame(&mut self) -> Option<(f64, &ffmpeg::frame::Video)> {
        loop {
            // Drain any frames the decoder already has buffered from prior packets.
            while self.decoder.receive_frame(&mut self.raw).is_ok() {
                let pts = self
                    .raw
                    .timestamp()
                    .map(|t| t as f64 * self.tb_f64)
                    .unwrap_or(-1.0);
                if pts < 0.0 || pts < self.min_pts {
                    continue; // negative/pre-seek-target frame → decode but skip.
                }
                self.scaler.run(&self.raw, &mut self.rgba).ok()?;
                return Some((pts, &self.rgba));
            }

            if self.eof {
                return None;
            }

            // Feed the next packet for our stream (one av_read_frame at a time).
            let mut packet = ffmpeg::Packet::empty();
            match packet.read(&mut self.ictx) {
                Ok(()) => {
                    if packet.stream() == self.stream_index {
                        let _ = self.decoder.send_packet(&packet);
                    }
                }
                Err(ffmpeg::Error::Eof) => {
                    let _ = self.decoder.send_eof();
                    self.eof = true; // drain remaining buffered frames, then stop.
                }
                Err(_) => {
                    self.eof = true;
                    let _ = self.decoder.send_eof();
                }
            }
        }
    }
}

/// Worker thread: decode frames in-process via `ffmpeg-next`, pace each to the
/// audio playhead by its REAL PTS, and frame-accurately re-seek the open
/// container whenever the transport reports a discontinuity (no respawn).
#[allow(clippy::too_many_arguments)]
fn worker_loop<C: VideoClock>(
    path: PathBuf,
    clock: C,
    slot: Arc<FrameSlot>,
    running: Arc<AtomicBool>,
    offset_ms: Arc<AtomicI64>,
    w: u32,
    h: u32,
    fps: f64,
) {
    ensure_ffmpeg_init();

    let mut epoch = clock.epoch();
    let start_t = clock.pos_secs().max(0.0);

    let mut dec = match Decoder::open(&path, w, h) {
        Some(d) => d,
        None => {
            eprintln!("video: in-process decoder open failed for {}", path.display());
            return;
        }
    };
    dec.seek(start_t);

    // `pending` holds the next decoded frame (pts, rgba bytes) waiting to be paced
    // in: we decode AHEAD of the playhead by one frame, present it when the
    // playhead reaches its pts, and only pull the next once it's consumed.
    let mut pending: Option<(f64, Vec<u8>)> = pull_packed(&mut dec, w, h);

    // Continuous-playhead reconstruction: anchor to the raw (stepped) audio clock
    // when it advances, extrapolate with wall-clock in between (capped). Kills the
    // step-induced judder a coarse audio-callback playhead would otherwise cause.
    let mut sm_raw = clock.pos_secs();
    let mut sm_wall = Instant::now();

    while running.load(Ordering::Acquire) {
        // --- handle a discontinuity (seek / load) → frame-accurate re-seek ---
        let cur_epoch = clock.epoch();
        if cur_epoch != epoch {
            epoch = cur_epoch;
            let target = clock.pos_secs().max(0.0);
            dec.seek(target); // INSTANT on the open container — the big win.
            pending = pull_packed(&mut dec, w, h);
            // Re-anchor the smoothing so we don't snap-drop right after the seek.
            sm_raw = clock.pos_secs();
            sm_wall = Instant::now();
            continue;
        }

        // Content time of the frame currently waiting to be presented.
        let (vt, _) = match pending {
            Some((pts, _)) => (pts, ()),
            None => {
                // EOF (or decode exhausted) → hold the last frame, idle until seek.
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
        };

        // Smooth the stepped audio clock into a continuous playhead.
        let raw = clock.pos_secs();
        let now = Instant::now();
        if raw > sm_raw + 1e-6 || raw < sm_raw - 0.05 {
            sm_raw = raw; // advanced a step (or seeked) → re-anchor
            sm_wall = now;
        }
        let smoothed = sm_raw + (now - sm_wall).as_secs_f64().min(SMOOTH_MAX_EXTRAP);
        // Compensate the display pipeline: aim `offset` ahead so the frame is
        // presented early enough to land on screen in sync with the audio.
        let offset = offset_ms.load(Ordering::Relaxed) as f64 / 1000.0;
        let ph = smoothed + offset;

        // Present when the (compensated) playhead reaches vt; drop stale frames if
        // the audio ran ahead; hold (sleep) if we're early or paused.
        let lead = ph - vt; // >0: audio ahead of this frame; <0: frame is early.

        // Fell too far behind (drift, or a heavy stretch the worker couldn't keep
        // up with) → snap to the playhead with one re-seek, not hundreds of drops.
        if lead > MAX_DRIFT_SECS {
            dec.seek(ph.max(0.0));
            pending = pull_packed(&mut dec, w, h);
            continue;
        }

        if lead < -0.5 / fps {
            // Frame is in the future (or transport paused/behind) → hold it, wait.
            std::thread::sleep(Duration::from_millis(8));
            continue;
        }

        // Audio is well ahead (> DROP_TOL): the pending frame is stale — discard it
        // and pull the next to catch up, content-accurately, WITHOUT presenting.
        // Small jitter does NOT drop (avoids periodic freezes).
        if lead > DROP_TOL_SECS {
            pending = pull_packed(&mut dec, w, h);
            continue;
        }

        // In the present window → show the pending frame, then pull the next.
        if let Some((pts, bytes)) = pending.take() {
            present_frame(&slot, &bytes, w, h, pts);
        }
        pending = pull_packed(&mut dec, w, h);
    }
    // `dec` drops here → container/decoder/scaler freed (RAII).
}

/// Pull one decoded+scaled frame from `dec` and copy its RGBA plane into a tight
/// `w*h*4` buffer (stripping swscale's row-stride padding). Returns `None` at EOF.
fn pull_packed(dec: &mut Decoder, w: u32, h: u32) -> Option<(f64, Vec<u8>)> {
    let (pts, frame) = dec.next_frame()?;
    let bytes = pack_rgba(frame, w, h);
    Some((pts, bytes))
}

/// Copy an RGBA `frame`'s plane 0 into a tight `w*h*4` `Vec`, stripping the
/// swscale row-stride padding (`stride` may exceed `w*4`).
fn pack_rgba(frame: &ffmpeg::frame::Video, w: u32, h: u32) -> Vec<u8> {
    let row_bytes = w as usize * 4;
    let frame_bytes = row_bytes * h as usize;
    let data = frame.data(0);
    let stride = frame.stride(0);

    let mut packed: Vec<u8> = Vec::with_capacity(frame_bytes);
    if stride == row_bytes {
        packed.extend_from_slice(&data[..frame_bytes.min(data.len())]);
    } else {
        for y in 0..h as usize {
            let start = y * stride;
            packed.extend_from_slice(&data[start..start + row_bytes]);
        }
    }
    packed
}

/// Copy one RGBA frame into the shared slot (reusing its buffer when the size
/// matches) and bump the generation so the GUI re-uploads it.
fn present_frame(slot: &FrameSlot, buf: &[u8], w: u32, h: u32, pts: f64) {
    if let Ok(mut g) = slot.frame.lock() {
        match g.as_mut() {
            Some(fr) if fr.width == w && fr.height == h => fr.rgba.copy_from_slice(buf),
            _ => {
                *g = Some(VideoFrame { width: w, height: h, rgba: buf.to_vec() });
            }
        }
    }
    slot.pts_bits.store(pts.to_bits(), Ordering::Release);
    slot.generation.fetch_add(1, Ordering::Release);
}

/// Scale `(w, h)` to fit within `(max_w, max_h)`, only downscaling, even dims ≥2.
fn fit_box(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let s = (max_w as f64 / w.max(1) as f64)
        .min(max_h as f64 / h.max(1) as f64)
        .min(1.0);
    let sw = (((w as f64 * s) as u32).max(2)) & !1;
    let sh = (((h as f64 * s) as u32).max(2)) & !1;
    (sw, sh)
}

/// `ffprobe` the first video stream for width / height / fps. Returns `None` for
/// audio-only files, missing streams, or any parse failure.
fn ffprobe(path: &PathBuf) -> Option<Probe> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,avg_frame_rate",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // CSV line: `WIDTH,HEIGHT,NUM/DEN` (empty when there is no video stream).
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    let mut it = line.splitn(3, ',');
    let width: u32 = it.next()?.trim().parse().ok()?;
    let height: u32 = it.next()?.trim().parse().ok()?;
    let rate = it.next()?.trim(); // "num/den"
    let fps = parse_rational(rate);
    if width == 0 || height == 0 {
        return None;
    }
    Some(Probe { width, height, fps })
}

/// Parse ffprobe's rational frame-rate string `"num/den"` → f64. Guards the
/// `0/0` (VFR/undefined) case by returning `0.0`, which the caller defaults.
fn parse_rational(s: &str) -> f64 {
    match s.split_once('/') {
        Some((n, d)) => {
            let n: f64 = n.trim().parse().unwrap_or(0.0);
            let d: f64 = d.trim().parse().unwrap_or(0.0);
            if d == 0.0 {
                0.0
            } else {
                n / d
            }
        }
        None => s.trim().parse().unwrap_or(0.0),
    }
}

#[cfg(test)]
mod viz_tests {
    use super::beat_intensity;

    // Even 120 BPM grid: a beat every 0.5 s.
    fn grid() -> Vec<f64> {
        (0..16).map(|n| n as f64 * 0.5).collect()
    }

    #[test]
    fn peaks_on_the_beat() {
        let g = grid();
        assert!((beat_intensity(&g, 1.0, 120.0) - 1.0).abs() < 1e-3); // exactly on beat 2
    }

    #[test]
    fn decays_between_beats() {
        let g = grid();
        let on = beat_intensity(&g, 1.0, 120.0);
        let mid = beat_intensity(&g, 1.25, 120.0); // a quarter beat later
        let late = beat_intensity(&g, 1.45, 120.0); // nearly the next beat
        assert!(mid < on);
        assert!(late < mid);
        assert!(late >= 0.0);
    }

    #[test]
    fn zero_before_first_beat_or_empty() {
        let g = grid();
        assert_eq!(beat_intensity(&g, -0.3, 120.0), 0.0);
        assert_eq!(beat_intensity(&[], 1.0, 120.0), 0.0);
    }
}
