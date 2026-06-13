//! M1b egui transport — the first window. Load a file (decoded on a worker so
//! the UI never blocks), play/pause, scrub a seek slider bound to the atomic
//! playhead, and trim deck/mic gain. Waveform + beat ticks arrive in M2.5.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};
use livehub_core::OutputRouting;
use livehub_engine::{Engine, LinkHub, TrackBuf};

use crate::video::VideoDeck;

/// Launch the GUI. Builds the audio engine first, then hands it to eframe.
pub fn run(
    device: &str,
    aux: Option<(usize, usize)>,
    open: Option<PathBuf>,
    autoplay: bool,
) -> anyhow::Result<()> {
    let engine = Engine::new(device, OutputRouting::default(), aux)?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([640.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native(
        "livehub",
        options,
        Box::new(move |_cc| -> Result<Box<dyn eframe::App>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(Box::new(App::new(engine, open, autoplay)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

struct App {
    engine: Engine,
    link: LinkHub,
    /// Pending native file-dialog result (dialog runs on its own thread).
    file_rx: Option<Receiver<Option<PathBuf>>>,
    deck_gain: f32,
    mic_gain: f32,
    aux_gain: f32,
    /// The video deck whose displayed frame = f(audio playhead). `None` until a
    /// file with a video stream is loaded.
    video: Option<VideoDeck>,
    /// GPU texture reused across frames; re-`set` only when the deck's frame
    /// generation changes (no realloc, no leak).
    video_tex: Option<egui::TextureHandle>,
    /// Last frame generation uploaded to `video_tex`.
    last_gen: u64,
    /// File to auto-load on the first frame (from `--open`).
    pending_open: Option<PathBuf>,
    /// Start playback once the auto-loaded file has decoded (`--play`).
    autoplay_pending: bool,
    // GUI render-time instrumentation (logged to spot repaint hitches).
    gui_frames: u64,
    gui_slow: u64,
    gui_maxdt: f32,
}

impl App {
    fn new(engine: Engine, open: Option<PathBuf>, autoplay: bool) -> Self {
        Self {
            engine,
            link: LinkHub::new(120.0),
            file_rx: None,
            deck_gain: 1.0,
            mic_gain: 1.0,
            aux_gain: 1.0,
            video: None,
            video_tex: None,
            last_gen: 0,
            pending_open: open,
            autoplay_pending: autoplay,
            gui_frames: 0,
            gui_slow: 0,
            gui_maxdt: 0.0,
        }
    }

    fn open_file_dialog(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .add_filter("audio", &["mp3", "wav", "flac", "m4a", "aac", "ogg"])
                .add_filter("video", &["mp4", "mov", "m4v", "mkv", "webm", "avi"])
                .pick_file();
            let _ = tx.send(picked);
        });
        self.file_rx = Some(rx);
    }

    /// Load `path` into the engine (its AUDIO drives the playhead) and, if it has
    /// a video stream, spin up a `VideoDeck` whose worker paces frames to that
    /// playhead. Dropping the old deck first stops its worker (kills ffmpeg).
    fn load_path(&mut self, path: PathBuf) {
        self.video = None; // drop old deck → joins worker, kills its ffmpeg.
        self.video_tex = None;
        self.last_gen = 0;
        self.video = VideoDeck::open(path.clone(), self.engine.transport());
        self.engine.load_track_background(path);
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // GUI render-time instrumentation: every ~120 repaints, log how many were
        // slow (>25ms = below 40fps) and the worst frame, to spot render hitches.
        let dt = ctx.input(|i| i.unstable_dt);
        self.gui_frames += 1;
        if dt > 0.025 {
            self.gui_slow += 1;
        }
        self.gui_maxdt = self.gui_maxdt.max(dt);
        if self.gui_frames.is_multiple_of(120) {
            eprintln!(
                "gui: {} frames, slow(>25ms) {}, max_dt {:.0}ms",
                self.gui_frames,
                self.gui_slow,
                self.gui_maxdt * 1000.0
            );
            self.gui_maxdt = 0.0;
        }

        // Auto-load a file passed via --open (once, on the first frame).
        if let Some(path) = self.pending_open.take() {
            self.load_path(path);
        }

        // Collect a finished file-dialog pick.
        if let Some(rx) = &self.file_rx {
            if let Ok(result) = rx.try_recv() {
                if let Some(path) = result {
                    self.load_path(path);
                }
                self.file_rx = None;
            }
        }

        // Pull the latest video frame into the GPU texture, but only when the
        // worker reports a new generation (no per-repaint re-upload).
        if let Some(deck) = &self.video {
            let gen = deck.generation();
            if gen != self.last_gen {
                let img = deck.with_latest(|fr| {
                    egui::ColorImage::from_rgba_unmultiplied(
                        [fr.width as usize, fr.height as usize],
                        &fr.rgba,
                    )
                });
                if let Some(img) = img {
                    match &mut self.video_tex {
                        Some(h) => h.set(img, egui::TextureOptions::LINEAR),
                        None => {
                            self.video_tex = Some(ctx.load_texture(
                                "video",
                                img,
                                egui::TextureOptions::LINEAR,
                            ));
                        }
                    }
                    self.last_gen = gen;
                }
            }
        }

        let dur = self.engine.duration_secs();
        let loading = self.engine.is_loading();

        // --play: start once the auto-loaded file has finished decoding.
        if self.autoplay_pending && !loading && dur > 0.0 {
            self.engine.play();
            self.autoplay_pending = false;
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("livehub");
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                if ui.button("📂 Load").clicked() && self.file_rx.is_none() {
                    self.open_file_dialog();
                }
                let playing = self.engine.is_playing();
                let label = if playing { "⏸ Pause" } else { "▶ Play" };
                if ui.button(label).clicked() {
                    self.engine.toggle();
                }
                if loading {
                    ui.spinner();
                    ui.label("decoding…");
                }
            });

            ui.add_space(8.0);

            // Video frame (frame = f(audio playhead)). Fit inside avail-width ×
            // max-height, aspect kept, so a vertical reels clip doesn't blow up
            // the window. Centered horizontally.
            if let Some(tex) = &self.video_tex {
                let [tw, th] = tex.size();
                if tw > 0 && th > 0 {
                    const MAX_H: f32 = 300.0;
                    let avail_w = ui.available_width();
                    let scale = (avail_w / tw as f32).min(MAX_H / th as f32);
                    let size = egui::vec2(tw as f32 * scale, th as f32 * scale);
                    ui.vertical_centered(|ui| ui.image((tex.id(), size)));
                    ui.add_space(8.0);
                }
            }

            // Waveform strip + beat ticks + playhead (M2.5). One cheap Arc clone
            // per frame; click the strip to seek.
            let track = self.engine.current_track();
            let playhead = self.engine.playhead_secs();
            if let Some(seek) = draw_waveform(ui, track.as_deref(), dur, playhead) {
                self.engine.seek_secs(seek);
            }

            // BPM readout (detected at load time).
            let bpm = self.engine.bpm();
            if bpm > 0.0 {
                ui.label(format!("{bpm:.1} BPM"));
            } else {
                ui.label("— BPM");
            }

            // Seek slider follows the playhead; dragging it seeks.
            let mut ph = self.engine.playhead_secs();
            let resp = ui.add(
                egui::Slider::new(&mut ph, 0.0..=dur.max(0.001))
                    .show_value(false)
                    .text("position"),
            );
            if resp.dragged() || resp.changed() {
                self.engine.seek_secs(ph);
            }
            ui.label(format!("{:>6.1} / {:>6.1} s", self.engine.playhead_secs(), dur));

            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.label("deck");
                if ui
                    .add(egui::Slider::new(&mut self.deck_gain, 0.0..=1.5))
                    .changed()
                {
                    self.engine.set_deck_gain(self.deck_gain);
                }
            });
            ui.horizontal(|ui| {
                ui.label("mic ");
                if ui
                    .add(egui::Slider::new(&mut self.mic_gain, 0.0..=1.5))
                    .changed()
                {
                    self.engine.set_mic_gain(self.mic_gain);
                }
            });
            if self.engine.has_aux() {
                ui.horizontal(|ui| {
                    ui.label("aux ");
                    if ui
                        .add(egui::Slider::new(&mut self.aux_gain, 0.0..=1.5))
                        .changed()
                    {
                        self.engine.set_aux_gain(self.aux_gain);
                    }
                });
            }
            // Video↔audio sync trim: drag until the picture sits on the beat. The
            // pipeline latency is machine/display specific, so it's a live knob.
            if let Some(deck) = &self.video {
                ui.horizontal(|ui| {
                    ui.label("video sync");
                    let mut off = deck.offset_ms();
                    if ui
                        .add(egui::Slider::new(&mut off, -200..=400).suffix(" ms"))
                        .changed()
                    {
                        deck.set_offset_ms(off);
                    }
                });
            }

            ui.add_space(8.0);
            ui.separator();
            ui.label("⛓ Ableton Link");
            ui.horizontal(|ui| {
                ui.label(format!("peers: {}", self.link.num_peers()));
                let mut bpm = self.link.tempo();
                if ui
                    .add(
                        egui::DragValue::new(&mut bpm)
                            .speed(0.1)
                            .range(20.0..=300.0)
                            .suffix(" BPM"),
                    )
                    .changed()
                {
                    self.link.set_tempo(bpm);
                }
                let playing = self.link.is_playing();
                if ui.button(if playing { "⏸ stop" } else { "▶ start" }).clicked() {
                    self.link.set_playing(!playing);
                }
            });
            let mut sss = self.link.start_stop_sync();
            if ui
                .checkbox(&mut sss, "transport sync (start/stop shared with peers)")
                .changed()
            {
                self.link.set_start_stop_sync(sss);
            }
            let q = self.link.quantum();
            let phase = self.link.beat_phase();
            let beat_num = phase.floor() as u32 + 1;
            ui.add(
                egui::ProgressBar::new((phase / q) as f32)
                    .text(format!("beat {} / {}", beat_num, q as u32)),
            );

            ui.add_space(6.0);
            ui.separator();
            ui.label(format!(
                "device {} Hz · underruns {}",
                self.engine.device_sr(),
                self.engine.underruns()
            ));
        });

        // Keep the playhead/meters live.
        ctx.request_repaint();
    }
}

/// Draw the deck waveform strip: peak envelope, beat ticks (downbeats brighter
/// and thicker), and a moving playhead. Returns a seek target in seconds when
/// the user clicks the strip. Handles `None`/empty/zero-duration without panic.
fn draw_waveform(
    ui: &mut egui::Ui,
    track: Option<&TrackBuf>,
    duration: f64,
    playhead: f64,
) -> Option<f64> {
    let (response, painter) =
        ui.allocate_painter(Vec2::new(ui.available_width(), 80.0), Sense::click());
    let rect: Rect = response.rect;

    painter.rect_filled(rect, 0.0, Color32::from_gray(20));

    let map_x = |nx: f32| rect.left() + nx.clamp(0.0, 1.0) * rect.width();

    let track = track?;
    let dur = duration.max(f64::MIN_POSITIVE);

    // Reserve a thin beat lane at the bottom; the waveform owns the area above it
    // so grid ticks never cross it.
    let lane_h = 14.0_f32;
    let wave_bottom = rect.bottom() - lane_h;
    let wave_mid = (rect.top() + wave_bottom) * 0.5;
    let wave_half = (wave_bottom - rect.top()) * 0.5;
    let map_y = |v: f32| wave_mid - v.clamp(-1.0, 1.0) * wave_half;

    // Waveform — clean, nothing drawn over it.
    let wave_stroke = Stroke::new(1.0, Color32::from_rgb(120, 200, 255));
    let n = track.waveform.len().max(1);
    for (i, &(lo, hi)) in track.waveform.iter().enumerate() {
        let x = map_x(i as f32 / n as f32);
        painter.line_segment([Pos2::new(x, map_y(hi)), Pos2::new(x, map_y(lo))], wave_stroke);
    }

    // Beat lane at the bottom: a short tick per beat, taller brighter ticks on
    // downbeats — a rhythm ruler that never touches the waveform.
    let beat_stroke = Stroke::new(1.0, Color32::from_gray(120));
    let beat_top = rect.bottom() - 5.0;
    for &t in &track.beats {
        let x = map_x((t / dur) as f32);
        painter.line_segment([Pos2::new(x, beat_top), Pos2::new(x, rect.bottom())], beat_stroke);
    }
    let down_stroke = Stroke::new(1.5, Color32::from_rgb(255, 205, 110));
    let down_top = rect.bottom() - lane_h;
    for &t in &track.downbeats {
        let x = map_x((t / dur) as f32);
        painter.line_segment([Pos2::new(x, down_top), Pos2::new(x, rect.bottom())], down_stroke);
    }

    // Playhead cursor on top of everything (full height).
    let px = map_x((playhead / dur) as f32);
    painter.line_segment(
        [Pos2::new(px, rect.top()), Pos2::new(px, rect.bottom())],
        Stroke::new(2.0, Color32::RED),
    );

    // Click-to-seek (guard against a zero-width strip → NaN).
    if response.clicked() && rect.width() > 0.0 {
        if let Some(p) = response.interact_pointer_pos() {
            let frac = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64;
            return Some(frac * dur);
        }
    }
    None
}
