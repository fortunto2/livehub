//! M4 egui transport — TWO independent decks (A/B), each with its own audio
//! playhead + video, an equal-power crossfader between them, and a video
//! composite (deck A under deck B, blended by the crossfader). Mic, aux and
//! Ableton Link stay shared. Files decode on a worker so the UI never blocks.

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
    open_b: Option<PathBuf>,
    autoplay: bool,
) -> anyhow::Result<()> {
    let engine = Engine::new(device, OutputRouting::default(), aux)?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([960.0, 820.0]),
        ..Default::default()
    };
    eframe::run_native(
        "livehub",
        options,
        Box::new(move |_cc| -> Result<Box<dyn eframe::App>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(Box::new(App::new(engine, open, open_b, autoplay)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

struct App {
    engine: Engine,
    link: LinkHub,
    /// Per-deck pending native file-dialog result (dialog runs on its own thread).
    file_rx: [Option<Receiver<Option<PathBuf>>>; 2],
    /// Per-deck gain (pre-crossfader).
    deck_gain: [f32; 2],
    /// Crossfader: 0.0 = full deck A, 1.0 = full deck B. Drives audio AND the
    /// video composite blend.
    crossfade: f32,
    mic_gain: f32,
    aux_gain: f32,
    /// Per-deck video deck; frame = f(that deck's audio playhead). `None` until a
    /// file with a video stream is loaded into that deck.
    video: [Option<VideoDeck>; 2],
    /// Per-deck GPU texture reused across frames; re-`set` only when the deck's
    /// frame generation changes (no realloc, no leak).
    video_tex: [Option<egui::TextureHandle>; 2],
    /// Last frame generation uploaded to each `video_tex`.
    last_gen: [u64; 2],
    /// Files to auto-load on the first frame (from `--open` / `--open-b`).
    pending_open: [Option<PathBuf>; 2],
    /// Start playback once the auto-loaded files have decoded (`--play`).
    autoplay_pending: bool,
    // GUI render-time instrumentation (logged to spot repaint hitches).
    gui_frames: u64,
    gui_slow: u64,
    gui_maxdt: f32,
}

impl App {
    fn new(
        engine: Engine,
        open: Option<PathBuf>,
        open_b: Option<PathBuf>,
        autoplay: bool,
    ) -> Self {
        Self {
            engine,
            link: LinkHub::new(120.0),
            file_rx: [None, None],
            deck_gain: [1.0, 1.0],
            crossfade: 0.5,
            mic_gain: 1.0,
            aux_gain: 1.0,
            video: [None, None],
            video_tex: [None, None],
            last_gen: [0, 0],
            pending_open: [open, open_b],
            autoplay_pending: autoplay,
            gui_frames: 0,
            gui_slow: 0,
            gui_maxdt: 0.0,
        }
    }

    fn open_file_dialog(&mut self, deck: usize) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .add_filter("audio", &["mp3", "wav", "flac", "m4a", "aac", "ogg"])
                .add_filter("video", &["mp4", "mov", "m4v", "mkv", "webm", "avi"])
                .pick_file();
            let _ = tx.send(picked);
        });
        self.file_rx[deck] = Some(rx);
    }

    /// Load `path` into `deck` (its AUDIO drives that deck's playhead) and, if it
    /// has a video stream, spin up a `VideoDeck` whose worker paces frames to that
    /// deck's playhead. Dropping the old deck first stops its worker (kills
    /// ffmpeg).
    fn load_path(&mut self, deck: usize, path: PathBuf) {
        self.video[deck] = None; // drop old deck → joins worker, kills its ffmpeg.
        self.video_tex[deck] = None;
        self.last_gen[deck] = 0;
        self.video[deck] = VideoDeck::open(path.clone(), self.engine.transport(deck));
        self.engine.load_track_background(deck, path);
    }

    /// Draw the A-over-B video composite into one fixed letterbox rect: deck A
    /// underneath, deck B on top, each tinted so the crossfader (`self.crossfade`)
    /// is a true alpha dissolve (A at `1-x`, B at `x`) — no double-bright midpoint.
    fn video_composite(&self, ui: &mut egui::Ui) {
        const MAX_H: f32 = 300.0;
        // Pick a composite size from whichever deck has a texture (fall back 16:9).
        let aspect = self
            .video_tex
            .iter()
            .flatten()
            .find_map(|t| {
                let [w, h] = t.size();
                (w > 0 && h > 0).then(|| w as f32 / h as f32)
            })
            .unwrap_or(16.0 / 9.0);
        let avail_w = ui.available_width();
        let (mut cw, mut ch) = (avail_w, avail_w / aspect);
        if ch > MAX_H {
            ch = MAX_H;
            cw = MAX_H * aspect;
        }
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(avail_w, ch), egui::Sense::hover());
        // Center the letterboxed composite within the allocated row.
        let composite =
            Rect::from_center_size(rect.center(), egui::vec2(cw.min(avail_w), ch));

        let xb = self.crossfade.clamp(0.0, 1.0);
        let a_alpha = ((1.0 - xb) * 255.0) as u8;
        let b_alpha = (xb * 255.0) as u8;
        if let Some(tex) = &self.video_tex[0] {
            egui::Image::new((tex.id(), composite.size()))
                .tint(Color32::from_white_alpha(a_alpha))
                .paint_at(ui, composite);
        }
        if let Some(tex) = &self.video_tex[1] {
            egui::Image::new((tex.id(), composite.size()))
                .tint(Color32::from_white_alpha(b_alpha))
                .paint_at(ui, composite);
        }
        ui.add_space(8.0);
    }

    /// Draw one deck's controls (Load / Play, waveform+beats, BPM, seek, gain,
    /// video sync) into `ui`, driving the deck-indexed engine methods.
    fn deck_panel(&mut self, ui: &mut egui::Ui, d: usize) {
        let dur = self.engine.duration_secs(d);
        let loading = self.engine.is_loading(d);

        ui.horizontal(|ui| {
            ui.strong(if d == 0 { "Deck A" } else { "Deck B" });
            if ui.button("📂 Load").clicked() && self.file_rx[d].is_none() {
                self.open_file_dialog(d);
            }
            let label = if self.engine.is_playing(d) { "⏸" } else { "▶" };
            if ui.button(label).clicked() {
                self.engine.toggle(d);
            }
            if loading {
                ui.spinner();
            }
        });

        // Waveform strip + beat ticks + playhead. One cheap Arc clone per frame;
        // click the strip to seek this deck.
        let track = self.engine.current_track(d);
        let playhead = self.engine.playhead_secs(d);
        if let Some(seek) = draw_waveform(ui, track.as_deref(), dur, playhead) {
            self.engine.seek_secs(d, seek);
        }

        // BPM readout (detected at load time).
        let bpm = self.engine.bpm(d);
        if bpm > 0.0 {
            ui.label(format!("{bpm:.1} BPM"));
        } else {
            ui.label("— BPM");
        }

        // Seek slider follows the playhead; dragging it seeks.
        let mut ph = self.engine.playhead_secs(d);
        let resp = ui.add(
            egui::Slider::new(&mut ph, 0.0..=dur.max(0.001))
                .show_value(false)
                .text("position"),
        );
        if resp.dragged() || resp.changed() {
            self.engine.seek_secs(d, ph);
        }
        ui.label(format!("{:>6.1} / {:>6.1} s", self.engine.playhead_secs(d), dur));

        ui.horizontal(|ui| {
            ui.label("gain");
            if ui
                .add(egui::Slider::new(&mut self.deck_gain[d], 0.0..=1.5))
                .changed()
            {
                self.engine.set_deck_gain(d, self.deck_gain[d]);
            }
        });

        // Video↔audio sync trim: drag until the picture sits on the beat. The
        // pipeline latency is machine/display specific, so it's a live knob.
        if let Some(deck) = &self.video[d] {
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

        // Auto-load files passed via --open / --open-b (once, on the first frame).
        for d in 0..2 {
            if let Some(path) = self.pending_open[d].take() {
                self.load_path(d, path);
            }
        }

        // Collect finished file-dialog picks, per deck.
        for d in 0..2 {
            if let Some(rx) = &self.file_rx[d] {
                if let Ok(result) = rx.try_recv() {
                    if let Some(path) = result {
                        self.load_path(d, path);
                    }
                    self.file_rx[d] = None;
                }
            }
        }

        // Pull the latest video frame into each deck's GPU texture, but only when
        // its worker reports a new generation (no per-repaint re-upload).
        for d in 0..2 {
            if let Some(deck) = &self.video[d] {
                let gen = deck.generation();
                if gen != self.last_gen[d] {
                    let img = deck.with_latest(|fr| {
                        egui::ColorImage::from_rgba_unmultiplied(
                            [fr.width as usize, fr.height as usize],
                            &fr.rgba,
                        )
                    });
                    if let Some(img) = img {
                        let name = if d == 0 { "video_a" } else { "video_b" };
                        match &mut self.video_tex[d] {
                            Some(h) => h.set(img, egui::TextureOptions::LINEAR),
                            None => {
                                self.video_tex[d] = Some(ctx.load_texture(
                                    name,
                                    img,
                                    egui::TextureOptions::LINEAR,
                                ));
                            }
                        }
                        self.last_gen[d] = gen;
                    }
                }
            }
        }

        let loading = self.engine.is_loading(0) || self.engine.is_loading(1);

        // --play: start each loaded deck once its file has finished decoding.
        if self.autoplay_pending && !loading {
            let mut any = false;
            for d in 0..2 {
                if self.engine.duration_secs(d) > 0.0 {
                    self.engine.play(d);
                    any = true;
                }
            }
            if any {
                self.autoplay_pending = false;
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("livehub");
            ui.add_space(6.0);

            // Video COMPOSITE: deck A under deck B, blended by the crossfader.
            self.video_composite(ui);

            // Two deck strips side by side.
            ui.columns(2, |cols| {
                self.deck_panel(&mut cols[0], 0);
                self.deck_panel(&mut cols[1], 1);
            });

            ui.add_space(6.0);
            // The crossfader, full width, between the decks and the shared section.
            if ui
                .add(
                    egui::Slider::new(&mut self.crossfade, 0.0..=1.0)
                        .show_value(false)
                        .text("A  ⟷  B"),
                )
                .changed()
            {
                self.engine.set_crossfade(self.crossfade);
            }

            ui.add_space(8.0);
            ui.separator();

            // Shared mic / aux gain.
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
