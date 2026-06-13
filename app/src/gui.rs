//! M4 egui transport — TWO independent decks (A/B), each with its own audio
//! playhead + video, an equal-power crossfader between them, and a video
//! composite (deck A under deck B, blended by the crossfader). Mic, aux and
//! Ableton Link stay shared. Files decode on a worker so the UI never blocks.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};
use livehub_core::OutputRouting;
use livehub_engine::{Engine, LinkHub, ParamInfo, TrackBuf};

use crate::video::{CameraDeck, ImageStill, VideoDeck, VideoSource, Visualizer};

/// Loop sizes as `(bar-fraction label, beats)`. One bar = 4 beats, so `¼` = 1
/// beat: short enough for rolls, while reading musically. At 80 BPM these span
/// ~0.75 s (¼) to ~12 s (4 bars).
pub(crate) const LOOP_SIZES: [(&str, usize); 5] =
    [("¼", 1), ("½", 2), ("1", 4), ("2", 8), ("4", 16)];

/// Per-deck accent colour: A = blue, B = red. Used for deck headers, big BPM
/// readouts, the stacked-overview tints, and the channel faders — so a glance
/// always tells A from B.
const DECK_COLOR: [Color32; 2] =
    [Color32::from_rgb(80, 150, 235), Color32::from_rgb(235, 90, 90)];

/// Mixer column width (px). Narrow on purpose — two vertical faders + cue.
const MIXER_W: f32 = 150.0;

/// Output-stage (canvas) aspect, switchable live. `Auto` follows the loaded
/// clips' orientation; the fixed modes force a shape regardless of the sources.
#[derive(Clone, Copy, PartialEq)]
enum CanvasMode {
    Auto,
    Wide,   // 16:9 horizontal
    Tall,   // 9:16 vertical
    Square, // 1:1
}

impl CanvasMode {
    const ALL: [CanvasMode; 4] =
        [CanvasMode::Auto, CanvasMode::Wide, CanvasMode::Tall, CanvasMode::Square];

    fn label(self) -> &'static str {
        match self {
            CanvasMode::Auto => "Auto",
            CanvasMode::Wide => "16:9",
            CanvasMode::Tall => "9:16",
            CanvasMode::Square => "1:1",
        }
    }

    /// Resolve to a concrete width:height aspect. `Auto` inspects the loaded
    /// clips: all portrait → vertical 9:16, all landscape → 16:9, all square →
    /// 1:1, mixed orientations or nothing loaded → 16:9.
    fn aspect(self, video_tex: &[Option<egui::TextureHandle>; 2]) -> f32 {
        match self {
            CanvasMode::Wide => 16.0 / 9.0,
            CanvasMode::Tall => 9.0 / 16.0,
            CanvasMode::Square => 1.0,
            CanvasMode::Auto => {
                let (mut any, mut all_portrait, mut all_landscape, mut all_square) =
                    (false, true, true, true);
                for t in video_tex.iter().flatten() {
                    let [w, h] = t.size();
                    if w == 0 || h == 0 {
                        continue;
                    }
                    any = true;
                    let a = w as f32 / h as f32;
                    all_portrait &= a < 0.95;
                    all_landscape &= a > 1.05;
                    all_square &= (a - 1.0).abs() <= 0.05;
                }
                if !any || (!all_portrait && !all_landscape && !all_square) {
                    16.0 / 9.0 // nothing loaded, or mixed orientations → wide
                } else if all_square {
                    1.0
                } else if all_portrait {
                    9.0 / 16.0
                } else {
                    16.0 / 9.0
                }
            }
        }
    }
}

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
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 900.0]),
        ..Default::default()
    };
    eframe::run_native(
        "livehub",
        options,
        Box::new(move |cc| -> Result<Box<dyn eframe::App>, Box<dyn std::error::Error + Send + Sync>> {
            apply_console_style(&cc.egui_ctx);
            Ok(Box::new(App::new(engine, open, open_b, autoplay)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

/// Flat, high-contrast dark theme — Ableton/Linn vibe: a panel fill that's a
/// touch above pure black, bright text, subtle rounding, a hair more breathing
/// room between widgets. No skeuomorphism, no grey-on-grey.
fn apply_console_style(ctx: &egui::Context) {
    use egui::{CornerRadius, Stroke};
    let mut visuals = egui::Visuals::dark();
    let panel = Color32::from_gray(24);
    let widget = Color32::from_gray(38);
    let widget_hover = Color32::from_gray(52);
    let widget_active = Color32::from_gray(64);
    let radius = CornerRadius::same(4);
    visuals.panel_fill = panel;
    visuals.window_fill = panel;
    visuals.extreme_bg_color = Color32::from_gray(14); // slider troughs / text edits
    visuals.faint_bg_color = Color32::from_gray(30);
    visuals.override_text_color = Some(Color32::from_gray(225));
    visuals.widgets.noninteractive.bg_fill = panel;
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, Color32::from_gray(200));
    visuals.widgets.inactive.bg_fill = widget;
    visuals.widgets.inactive.weak_bg_fill = widget;
    visuals.widgets.inactive.corner_radius = radius;
    visuals.widgets.hovered.bg_fill = widget_hover;
    visuals.widgets.hovered.weak_bg_fill = widget_hover;
    visuals.widgets.hovered.corner_radius = radius;
    visuals.widgets.active.bg_fill = widget_active;
    visuals.widgets.active.weak_bg_fill = widget_active;
    visuals.widgets.active.corner_radius = radius;
    visuals.selection.bg_fill = DECK_COLOR[0].linear_multiply(0.55);
    visuals.selection.stroke = Stroke::new(1.0, DECK_COLOR[0]);
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    ctx.set_style(style);
}

struct App {
    engine: Engine,
    link: LinkHub,
    /// Per-deck pending native file-dialog result (dialog runs on its own thread).
    file_rx: [Option<Receiver<Option<PathBuf>>>; 2],
    /// Per-deck gain (pre-crossfader).
    deck_gain: [f32; 2],
    /// Per-deck playback rate (1.0 = original). Mirrors `engine.rate(d)`; the
    /// tempo slider writes it and SYNC reads the engine back into it each frame.
    deck_rate: [f32; 2],
    /// Crossfader: 0.0 = full deck A, 1.0 = full deck B. Drives audio AND the
    /// video composite blend.
    crossfade: f32,
    mic_gain: f32,
    aux_gain: f32,
    /// Shared headphone CUE / PFL bus level (pre-fader listen for cued decks).
    cue_gain: f32,
    /// Per-deck video source: a paced file deck (frame = f(playhead)), a live
    /// webcam, or a static image. `None` until something is loaded into that deck.
    video: [Option<VideoSource>; 2],
    /// Per-deck pending image-dialog result (dialog runs on its own thread).
    image_rx: [Option<Receiver<Option<PathBuf>>>; 2],
    /// Per-deck source media path (the file driving its audio), kept so karaoke
    /// knows what to split. Set by `load_path` and on a finished import; left
    /// `None` for camera/image/visualizer sources (they have no audio file).
    deck_path: [Option<PathBuf>; 2],
    /// Shared URL text box feeding the `⬇A` / `⬇B` import buttons (one URL, either
    /// deck) — the source is global, not per-deck.
    import_url: String,
    /// Per-deck pending yt-dlp download (`Ok(mp4 path)` or an error string). Worker
    /// runs on its own thread; drained in `update()`.
    import_rx: [Option<Receiver<Result<PathBuf, String>>>; 2],
    /// Per-deck pending demucs split (`Ok(no_vocals.wav)` or an error string).
    karaoke_rx: [Option<Receiver<Result<PathBuf, String>>>; 2],
    /// Per-deck background-task status label ("downloading…" / "splitting…"), shown
    /// as a spinner+label and used to gate the buttons while busy. `None` = idle.
    busy: [Option<&'static str>; 2],
    /// Whether `demucs` is installed (probed once at startup — the `-h` shell-out
    /// is too slow to run per-repaint). Gates the 🎤 Karaoke button.
    demucs_ok: bool,
    /// Whether `yt-dlp` is installed (probed once at startup). Gates the import
    /// buttons.
    ytdlp_ok: bool,
    /// Camera enumeration cached once at startup (`CameraDeck::list_cameras`
    /// spawns an ffmpeg probe, so we never call it per-repaint).
    cameras: Vec<String>,
    /// Per-deck GPU texture reused across frames; re-`set` only when the deck's
    /// frame generation changes (no realloc, no leak).
    video_tex: [Option<egui::TextureHandle>; 2],
    /// Last frame generation uploaded to each `video_tex`.
    last_gen: [u64; 2],
    /// Files to auto-load on the first frame (from `--open` / `--open-b`).
    pending_open: [Option<PathBuf>; 2],
    /// Start playback once the auto-loaded files have decoded (`--play`).
    autoplay_pending: bool,
    /// Snap seeks to the nearest beat when ON (global, applies to both decks).
    snap: bool,
    /// When ON, a separate borderless OUTPUT WINDOW shows only the live video
    /// composite (no controls) — fullscreen it on a second display for OBS to
    /// grab via Window/Display Capture.
    output_open: bool,
    /// Output-stage aspect (Auto / 16:9 / 9:16 / 1:1), switchable live; drives
    /// both the in-app composite and the output window.
    canvas_mode: CanvasMode,
    /// Per-deck engaged beat-loop size in beats, to highlight the active button.
    /// `None` = no loop on that deck.
    loop_len: [Option<usize>; 2],
    /// Installed `SuperDuper*.clap` bundles selectable as the master insert
    /// (scanned once at startup). The combo shows their basenames + "(none)".
    fx_choices: Vec<std::path::PathBuf>,
    /// Index into `fx_choices` of the loaded master FX, or `None` for "(none)".
    fx_selected: Option<usize>,
    /// Params of the loaded master FX (one slider each). Empty when none loaded.
    fx_params: Vec<ParamInfo>,
    /// Display name of the loaded master FX (for the section header).
    fx_name: Option<String>,
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
            image_rx: [None, None],
            deck_path: [None, None],
            import_url: String::new(),
            import_rx: [None, None],
            karaoke_rx: [None, None],
            busy: [None, None],
            demucs_ok: crate::import::demucs_available(),
            ytdlp_ok: crate::import::yt_dlp_available(),
            cameras: CameraDeck::list_cameras(),
            deck_gain: [1.0, 1.0],
            deck_rate: [1.0, 1.0],
            crossfade: 0.5,
            mic_gain: 1.0,
            aux_gain: 1.0,
            cue_gain: 1.0,
            video: [None, None],
            video_tex: [None, None],
            last_gen: [0, 0],
            pending_open: [open, open_b],
            autoplay_pending: autoplay,
            snap: true,
            output_open: false,
            canvas_mode: CanvasMode::Auto,
            loop_len: [None, None],
            fx_choices: Engine::list_fx(),
            fx_selected: None,
            fx_params: Vec::new(),
            fx_name: None,
            gui_frames: 0,
            gui_slow: 0,
            gui_maxdt: 0.0,
        }
    }

    /// Render the "Master FX" section: a combo to pick one installed
    /// `SuperDuper*.clap` as the master-bus insert (or "(none)"), then one slider
    /// per param. Selecting an effect rebuilds the output stream (brief gap) and
    /// stashes its param list; moving a slider pushes the change wait-free.
    fn master_fx_panel(&mut self, ui: &mut egui::Ui) {
        ui.label("🎛 Master FX");
        let current = self
            .fx_selected
            .and_then(|i| self.fx_choices.get(i))
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("(none)")
            .to_string();

        let mut pick: Option<Option<usize>> = None;
        egui::ComboBox::from_id_salt("master_fx_combo")
            .selected_text(current)
            .show_ui(ui, |ui| {
                if ui.selectable_label(self.fx_selected.is_none(), "(none)").clicked() {
                    pick = Some(None);
                }
                for (i, path) in self.fx_choices.iter().enumerate() {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("?");
                    if ui.selectable_label(self.fx_selected == Some(i), name).clicked() {
                        pick = Some(Some(i));
                    }
                }
            });

        if let Some(choice) = pick {
            // Only act on an actual change.
            if choice != self.fx_selected {
                let path = choice.and_then(|i| self.fx_choices.get(i).cloned());
                match self.engine.set_master_fx(path.as_deref()) {
                    Ok(params) => {
                        self.fx_selected = choice;
                        self.fx_params = params;
                        self.fx_name = choice.and_then(|i| {
                            self.fx_choices.get(i).and_then(|p| {
                                p.file_name().and_then(|n| n.to_str()).map(String::from)
                            })
                        });
                    }
                    Err(e) => eprintln!("set_master_fx failed: {e:#}"),
                }
            }
        }

        if let Some(name) = &self.fx_name {
            ui.weak(name);
        }
        // One slider per param. Pushing a change is wait-free (no rebuild).
        for p in &mut self.fx_params {
            let range = if p.max > p.min { p.min..=p.max } else { p.min..=(p.min + 1.0) };
            if ui
                .add(egui::Slider::new(&mut p.value, range).text(&p.name))
                .changed()
            {
                self.engine.set_fx_param(p.id, p.value);
            }
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
        self.reset_video(deck); // drop old source → joins worker, kills its ffmpeg.
        self.deck_path[deck] = Some(path.clone()); // remember the source for karaoke.
        self.video[deck] =
            VideoDeck::open(path.clone(), self.engine.transport(deck)).map(VideoSource::File);
        self.engine.load_track_background(deck, path);
    }

    /// Spawn the yt-dlp download worker for `deck`: download `self.import_url` to
    /// the import scratch dir on its own thread, sending `Result<PathBuf, String>`
    /// back over `import_rx[deck]`. Marks the deck busy; the drain in `update()`
    /// loads the finished mp4 via `load_path`. No-op if the URL is empty.
    fn start_import(&mut self, deck: usize) {
        let url = self.import_url.trim().to_string();
        if url.is_empty() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = crate::import::yt_dlp_download(&url, &crate::import::import_dir())
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        self.import_rx[deck] = Some(rx);
        self.busy[deck] = Some("downloading…");
    }

    /// Spawn the demucs karaoke-split worker for `deck`: split this deck's source
    /// file into vocals + instrumental on its own thread, sending the instrumental
    /// path (or an error) back over `karaoke_rx[deck]`. Marks the deck busy; the
    /// drain swaps the deck's AUDIO to the instrumental. No-op without a source.
    fn start_karaoke(&mut self, deck: usize) {
        let Some(input) = self.deck_path[deck].clone() else { return };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = crate::import::demucs_split(&input, &crate::import::import_dir())
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        self.karaoke_rx[deck] = Some(rx);
        self.busy[deck] = Some("splitting…");
    }

    /// Switch `deck`'s picture to a live webcam (camera `index`). Camera is
    /// video-only: it does NOT touch the deck's audio (the deck can still have a
    /// track loaded). Dropping the old source first stops its worker.
    fn load_camera(&mut self, deck: usize, index: usize) {
        self.reset_video(deck);
        self.video[deck] = CameraDeck::open(index).map(VideoSource::Camera);
    }

    /// Switch `deck`'s picture to a static image. Image has no audio, so unlike
    /// `load_path` this does not call `load_track_background` — the deck keeps
    /// whatever audio it had.
    fn load_image(&mut self, deck: usize, path: PathBuf) {
        self.reset_video(deck);
        self.video[deck] = ImageStill::open(&path).map(VideoSource::Image);
    }

    /// Switch `deck`'s picture to a beat-reactive visualizer locked to this deck's
    /// playhead + beatgrid. Needs a decoded track (for the beats); no-op otherwise.
    fn load_visualizer(&mut self, deck: usize) {
        let Some(track) = self.engine.current_track(deck) else { return };
        let (beats, bpm) = (track.beats.clone(), track.bpm);
        self.reset_video(deck);
        self.video[deck] = Some(VideoSource::Visualizer(Visualizer::open(
            self.engine.transport(deck),
            beats,
            bpm,
        )));
    }

    /// Drop the deck's current video source (joins its worker / kills its ffmpeg)
    /// and reset the GPU texture state, ready for a fresh source.
    fn reset_video(&mut self, deck: usize) {
        self.video[deck] = None;
        self.video_tex[deck] = None;
        self.last_gen[deck] = 0;
    }

    /// Open a native image-file picker on a worker thread; the result is drained in
    /// `update()` and routed to `load_image`.
    fn open_image_dialog(&mut self, deck: usize) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .add_filter("image", &["png", "jpg", "jpeg", "webp", "gif"])
                .pick_file();
            let _ = tx.send(picked);
        });
        self.image_rx[deck] = Some(rx);
    }

    /// Draw the A-over-B video composite into one fixed letterbox row of `ui`:
    /// deck A underneath, deck B on top, blended by the crossfader. Thin wrapper
    /// over [`paint_composite_into`] with the in-app 300 px height cap, plus the
    /// trailing `add_space` the layout below it expects.
    fn video_composite(&self, ui: &mut egui::Ui) {
        let aspect = self.canvas_mode.aspect(&self.video_tex);
        Self::paint_composite_into(ui, &self.video_tex, self.crossfade, aspect, Some(180.0));
        ui.add_space(8.0);
    }

    /// Stacked A/B match overview: one full-width rect split into a TOP half
    /// (deck A, blue) and BOTTOM half (deck B, red). Each half draws that deck's
    /// peak envelope, beat ticks, engaged-loop band, and a vertical playhead —
    /// the primary beat-match view, replacing the per-deck full waveforms.
    /// Clicking the top half seeks deck A, the bottom half seeks deck B (with
    /// beat-snap when `self.snap`).
    fn draw_stacked_overview(&mut self, ui: &mut egui::Ui, height: f32) {
        let (response, painter) =
            ui.allocate_painter(Vec2::new(ui.available_width(), height), Sense::click());
        let rect = response.rect;
        painter.rect_filled(rect, 4.0, Color32::from_gray(16));

        let half_h = rect.height() * 0.5;
        let lanes = [
            Rect::from_min_max(rect.left_top(), Pos2::new(rect.right(), rect.top() + half_h)),
            Rect::from_min_max(Pos2::new(rect.left(), rect.top() + half_h), rect.right_bottom()),
        ];
        // Hairline between the two lanes.
        painter.line_segment(
            [
                Pos2::new(rect.left(), rect.center().y),
                Pos2::new(rect.right(), rect.center().y),
            ],
            Stroke::new(1.0, Color32::from_gray(40)),
        );

        for d in 0..2 {
            let lane = lanes[d];
            let track = self.engine.current_track(d);
            let dur = self.engine.duration_secs(d);
            let playhead = self.engine.playhead_secs(d);
            let sr = track.as_deref().map(|t| t.sample_rate as f64).unwrap_or(1.0);
            let loop_region = self
                .engine
                .loop_region(d)
                .map(|(s, e)| (s as f64 / sr, e as f64 / sr));
            draw_overview_lane(
                &painter,
                lane,
                track.as_deref(),
                dur,
                playhead,
                loop_region,
                DECK_COLOR[d],
            );
        }

        // Click-to-seek: top half → deck A, bottom half → deck B.
        if response.clicked() && rect.width() > 0.0 {
            if let Some(p) = response.interact_pointer_pos() {
                let d = if p.y < rect.center().y { 0 } else { 1 };
                let dur = self.engine.duration_secs(d);
                let frac = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64;
                let seek = frac * dur.max(f64::MIN_POSITIVE);
                let track = self.engine.current_track(d);
                let target = match track.as_deref() {
                    Some(t) if self.snap && !t.beats.is_empty() => snap_to_beat(&t.beats, seek),
                    _ => seek,
                };
                self.engine.seek_secs(d, target);
            }
        }
    }

    /// Shared composite renderer used by BOTH the in-app strip and the standalone
    /// output window — one implementation of the A-over-B alpha-dissolve blend.
    ///
    /// `video_tex` are the two deck textures (deck 0 under deck 1). `crossfade`
    /// (0 = full A, 1 = full B) tints each layer so the fade is a true alpha
    /// dissolve (A at `1-x`, B at `x`) — no double-bright midpoint.
    ///
    /// A FIXED 16:9 canvas (the "stage") is centered in the row this allocates;
    /// each deck's video is then fit INTO that canvas at ITS OWN aspect ratio —
    /// landscape fills the width, a vertical reel pillarboxes against black — so
    /// mismatched-format clips are never stretched. `max_h = Some(h)` caps the
    /// row height (in-app strip); `max_h = None` fills `ui.available_size()` (the
    /// output window) so OBS captures a full-bleed 16:9 frame.
    fn paint_composite_into(
        ui: &mut egui::Ui,
        video_tex: &[Option<egui::TextureHandle>; 2],
        crossfade: f32,
        canvas_aspect: f32,
        max_h: Option<f32>,
    ) {
        // The canvas (stage) aspect is chosen by the user (Auto/16:9/9:16/1:1),
        // NOT derived per-clip — so A and B share one stage and each keeps its
        // own shape inside it.
        let avail_w = ui.available_width();
        let row_h = match max_h {
            Some(cap) => (avail_w / canvas_aspect).min(cap),
            None => ui.available_size().y.max(1.0),
        };
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(avail_w, row_h), egui::Sense::hover());

        // The canvas, aspect-fit + centered in the allocated row, painted black
        // so every clip letterboxes/pillarboxes against a clean stage.
        let canvas = fit_aspect(rect, canvas_aspect);
        ui.painter().rect_filled(canvas, 0.0, Color32::BLACK);

        let xb = crossfade.clamp(0.0, 1.0);
        let alpha = [((1.0 - xb) * 255.0) as u8, (xb * 255.0) as u8];
        // Deck 0 under deck 1; each fit into the canvas at its OWN aspect (no
        // stretch) and tinted so the crossfader is a true alpha dissolve.
        for (d, slot) in video_tex.iter().enumerate() {
            if let Some(tex) = slot {
                let [tw, th] = tex.size();
                if tw == 0 || th == 0 {
                    continue;
                }
                let fit = fit_aspect(canvas, tw as f32 / th as f32);
                egui::Image::new((tex.id(), fit.size()))
                    .tint(Color32::from_white_alpha(alpha[d]))
                    .paint_at(ui, fit);
            }
        }
    }

    /// Draw one deck's controls (Load / Play, waveform+beats, BPM, seek, gain,
    /// video sync) into `ui`, driving the deck-indexed engine methods.
    fn deck_panel(&mut self, ui: &mut egui::Ui, d: usize) {
        let dur = self.engine.duration_secs(d);
        let loading = self.engine.is_loading(d);

        let accent = DECK_COLOR[d];

        // Header: coloured deck label + source-kind tag.
        ui.horizontal(|ui| {
            ui.colored_label(accent, egui::RichText::new(if d == 0 {
                "DECK A"
            } else {
                "DECK B"
            }).strong());
            // Tiny tag showing the current picture source kind (file/cam/image).
            if let Some(src) = &self.video[d] {
                ui.weak(src.kind());
            }
            if loading {
                ui.spinner();
            }
            // Background-task (download / split) progress for this deck.
            if let Some(status) = self.busy[d] {
                ui.spinner();
                ui.weak(status);
            }
        });

        // BIG BPM readout — heading-size, deck-coloured. Shows the effective BPM
        // (grid × the deck's base rate) when off 1.00×, else the grid BPM.
        // `deck_rate` is UI-authoritative (slider / SYNC / reset write it); we do
        // NOT mirror the engine here, so a held nudge can't corrupt the base.
        let bpm = self.engine.bpm(d);
        let rate = self.deck_rate[d];
        let bpm_text = if bpm > 0.0 {
            if (rate - 1.0).abs() > 0.0005 {
                format!("{:.1} BPM", bpm * rate as f64)
            } else {
                format!("{bpm:.1} BPM")
            }
        } else {
            "— BPM".to_string()
        };
        ui.label(egui::RichText::new(bpm_text).color(accent).heading());

        // TRANSPORT — the main per-deck control, kept big + obvious and separate
        // from the source buttons: a coloured Play/Pause + a ⏮ cue-to-start.
        ui.horizontal(|ui| {
            let playing = self.engine.is_playing(d);
            let (label, fill) = if playing {
                ("⏸  Pause", Color32::from_gray(70))
            } else {
                ("▶  Play", accent)
            };
            if ui
                .add_sized(
                    [110.0, 30.0],
                    egui::Button::new(egui::RichText::new(label).strong().color(Color32::WHITE))
                        .fill(fill),
                )
                .clicked()
            {
                self.engine.toggle(d);
            }
            if ui
                .add_sized([42.0, 30.0], egui::Button::new("⏮"))
                .on_hover_text("cue to start")
                .clicked()
            {
                self.engine.seek_secs(d, 0.0);
            }
        });

        // Source buttons (what the deck shows / its audio source).
        ui.horizontal(|ui| {
            if ui.button("📂 Load").clicked() && self.file_rx[d].is_none() {
                self.open_file_dialog(d);
            }
            // 📷 = live webcam (index 0); only offered when a camera exists.
            if ui
                .add_enabled(!self.cameras.is_empty(), egui::Button::new("📷"))
                .on_hover_text("live camera")
                .clicked()
            {
                self.load_camera(d, 0);
            }
            // 🖼 = static image (file dialog on a worker thread).
            if ui.button("🖼").on_hover_text("still image").clicked()
                && self.image_rx[d].is_none()
            {
                self.open_image_dialog(d);
            }
            // 🌈 = beat-reactive visualizer; needs this deck's beatgrid (a track).
            if ui
                .add_enabled(
                    self.engine.current_track(d).is_some(),
                    egui::Button::new("🌈"),
                )
                .on_hover_text("beat visualizer")
                .clicked()
            {
                self.load_visualizer(d);
            }
            // 🎤 = OPTIONAL karaoke split (demucs): swap this deck's AUDIO to the
            // instrumental stem while its video keeps playing. Needs a source file
            // to split, demucs installed, and the deck idle. When demucs is missing
            // the button is disabled with an install hint.
            let can_karaoke =
                self.demucs_ok && self.deck_path[d].is_some() && self.busy[d].is_none();
            let karaoke_btn = ui.add_enabled(can_karaoke, egui::Button::new("🎤"));
            let karaoke_btn = if self.demucs_ok {
                karaoke_btn.on_hover_text("karaoke: split out vocals (demucs)")
            } else {
                karaoke_btn.on_disabled_hover_text("install demucs: uv tool install demucs")
            };
            if karaoke_btn.clicked() {
                self.start_karaoke(d);
            }
        });

        // (The full waveform now lives in the shared stacked overview above —
        // click its top/bottom half to seek this deck.)
        let track = self.engine.current_track(d);

        // Loop row: sizes are fractions/multiples of a bar (4 beats) so they read
        // musically (¼ bar = 1 beat) while resolving down to short loops. The loop
        // starts on the beat at-or-before the playhead; ✕ clears it.
        ui.horizontal(|ui| {
            ui.label("loop");
            for (label, k) in LOOP_SIZES {
                if ui
                    .selectable_label(self.loop_len[d] == Some(k), label)
                    .clicked()
                {
                    self.engage_loop(d, k);
                }
            }
            if ui.button("✕").clicked() {
                self.engine.clear_loop(d);
                self.loop_len[d] = None;
            }
        });

        // Seek slider follows the playhead; dragging it seeks. (Gain + cue live
        // in the centre MIXER column; deck gain is no longer drawn here.)
        let mut ph = self.engine.playhead_secs(d);
        let resp = ui.add(
            egui::Slider::new(&mut ph, 0.0..=dur.max(0.001))
                .show_value(false)
                .text("position"),
        );
        if resp.dragged() || resp.changed() {
            let target = match track.as_deref() {
                Some(t) if self.snap && !t.beats.is_empty() => snap_to_beat(&t.beats, ph),
                _ => ph,
            };
            self.engine.seek_secs(d, target);
        }
        ui.label(format!("{:>6.1} / {:>6.1} s", self.engine.playhead_secs(d), dur));

        // TEMPO row: ±8% vinyl-rate fader (the BASE rate) + ⟲ reset + NUDGE ◀/▶
        // (hold to pull the deck against the other for manual beatmatch) + ⇄ SYNC
        // (match the other deck's tempo AND beat phase). `deck_rate[d]` is the base;
        // a held nudge multiplies it momentarily without changing the base.
        ui.horizontal(|ui| {
            ui.label("tempo");
            ui.add(egui::Slider::new(&mut self.deck_rate[d], 0.92..=1.08).show_value(false));
            if ui.small_button("⟲").on_hover_text("reset to 1.00×").clicked() {
                self.deck_rate[d] = 1.0;
            }
            // NUDGE: hold ◀/▶ to temporarily slow/speed the deck so its beats slide
            // against the other deck (like nudging a CDJ jog). Releases back to base.
            let back = ui.button("◀").on_hover_text("nudge slower (hold)");
            let fwd = ui.button("▶").on_hover_text("nudge faster (hold)");
            let nudge = if fwd.is_pointer_button_down_on() {
                1.04
            } else if back.is_pointer_button_down_on() {
                0.96
            } else {
                1.0
            };
            // ⇄ SYNC: match the other deck's tempo AND beat phase (no clash).
            if ui
                .button("⇄ SYNC")
                .on_hover_text("match the other deck's tempo + beat phase")
                .clicked()
            {
                self.engine.sync_deck(d, 1 - d);
                self.deck_rate[d] = self.engine.rate(d); // reflect synced base
            }
            // Effective BPM = grid bpm × base × nudge.
            let eff = self.engine.bpm(d) * (self.deck_rate[d] * nudge) as f64;
            if eff > 0.0 {
                ui.monospace(format!("{eff:6.2}"));
            } else {
                ui.monospace(format!("{:.3}×", self.deck_rate[d] * nudge));
            }
            // Apply base × nudge every frame (cheap atomic store).
            self.engine.set_rate(d, self.deck_rate[d] * nudge);
        });

        // Video↔audio sync trim: drag until the picture sits on the beat. The
        // pipeline latency is machine/display specific, so it's a live knob. Only
        // a paced file deck has a meaningful offset — camera/image have no
        // playhead to compensate against, so the slider is hidden for them.
        if self.video[d].as_ref().is_some_and(|s| s.is_file()) {
            let source = self.video[d].as_ref().unwrap();
            ui.horizontal(|ui| {
                ui.label("video sync");
                let mut off = source.offset_ms();
                if ui
                    .add(egui::Slider::new(&mut off, -200..=400).suffix(" ms"))
                    .changed()
                {
                    source.set_offset_ms(off);
                }
            });
        }
    }

    /// Centre MIXER column: deck A and deck B GAIN as two tall vertical faders
    /// (tinted A/B), a 🎧 cue toggle under each fader, and the shared headphone
    /// CUE level. Narrow on purpose. Every control reaches the same engine method
    /// it did in the old horizontal layout.
    fn mixer_panel(&mut self, ui: &mut egui::Ui, fader_h: f32) {
        ui.vertical_centered(|ui| {
            ui.colored_label(Color32::from_gray(170), egui::RichText::new("MIX").strong());
        });
        ui.add_space(4.0);

        // Two vertical gain faders side by side. add_sized gives each an EXPLICIT
        // interactive rect (34×fader_h) — the old `slider_width` spacing trick left
        // the vertical sliders un-draggable. Gain pushed every frame + shown.
        ui.horizontal(|ui| {
            for (d, &accent) in DECK_COLOR.iter().enumerate() {
                ui.vertical(|ui| {
                    ui.colored_label(accent, if d == 0 { "A" } else { "B" });
                    ui.add_sized(
                        [34.0, fader_h],
                        egui::Slider::new(&mut self.deck_gain[d], 0.0..=1.5)
                            .vertical()
                            .show_value(false),
                    );
                    self.engine.set_deck_gain(d, self.deck_gain[d]); // push every frame
                    ui.label(egui::RichText::new(format!("{:.2}", self.deck_gain[d])).small());
                    // 🎧 cue toggle (pre-fader headphone listen) under the fader.
                    let mut cued = self.engine.is_cued(d);
                    if ui
                        .toggle_value(&mut cued, "🎧")
                        .on_hover_text("cue (pre-fader headphone listen)")
                        .changed()
                    {
                        self.engine.set_cue(d, cued);
                    }
                });
                if d == 0 {
                    ui.add_space(20.0);
                }
            }
        });

        ui.add_space(8.0);
        // Shared headphone CUE bus level.
        ui.label("cue");
        ui.add_sized(
            [MIXER_W - 12.0, 18.0],
            egui::Slider::new(&mut self.cue_gain, 0.0..=1.5).show_value(false),
        );
        self.engine.set_cue_gain(self.cue_gain);
    }

    /// Engage a `k`-beat loop on `deck` (the loop buttons pass `k` as beats; the
    /// label is the bar-fraction). The region — start on the beat at-or-before the
    /// playhead, exactly `k` beats long — comes from the shared pure helper, so the
    /// GUI and the headless `selftest-loops` agree. No-op if fewer than `k` beats
    /// remain ahead.
    fn engage_loop(&mut self, d: usize, k: usize) {
        let Some(track) = self.engine.current_track(d) else { return };
        let ph = self.engine.playhead_secs(d);
        if let Some((start, end)) =
            livehub_engine::beat_loop_region(&track.beats, ph, k, track.sample_rate)
        {
            self.engine.set_loop(d, start, end);
            self.loop_len[d] = Some(k);
        }
    }

    /// TOP BAR: title · BPM-A ⇄ BPM-B with a "matched" lamp (GREEN when the two
    /// decks' effective BPM are within 0.1) · canvas selector · Output · Snap.
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("livehub");
            ui.add_space(12.0);

            // BPM-A ⇄ BPM-B with a match lamp. Effective BPM = grid × live rate.
            let eff = [
                self.engine.bpm(0) * self.engine.rate(0) as f64,
                self.engine.bpm(1) * self.engine.rate(1) as f64,
            ];
            let fmt = |b: f64| if b > 0.0 { format!("{b:.1}") } else { "—".to_string() };
            ui.colored_label(DECK_COLOR[0], egui::RichText::new(fmt(eff[0])).strong());
            let matched = eff[0] > 0.0 && eff[1] > 0.0 && (eff[0] - eff[1]).abs() <= 0.1;
            let lamp = if matched {
                Color32::from_rgb(60, 220, 110)
            } else {
                Color32::from_gray(90)
            };
            ui.colored_label(lamp, "⇄");
            ui.colored_label(DECK_COLOR[1], egui::RichText::new(fmt(eff[1])).strong());

            // Right-aligned: canvas selector, Output, Snap.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.toggle_value(&mut self.snap, "⊞ Snap");
                ui.toggle_value(&mut self.output_open, "🖥 Output");
                ui.separator();
                // Canvas selector (Auto follows the loaded clips' orientation).
                if self.canvas_mode == CanvasMode::Auto {
                    let a = CanvasMode::Auto.aspect(&self.video_tex);
                    ui.weak(if a > 1.05 {
                        "→16:9"
                    } else if a < 0.95 {
                        "→9:16"
                    } else {
                        "→1:1"
                    });
                }
                // right_to_left reverses visual order, so iterate reversed to keep
                // Auto/16:9/9:16/1:1 reading left-to-right on screen.
                for m in CanvasMode::ALL.into_iter().rev() {
                    if ui.selectable_label(self.canvas_mode == m, m.label()).clicked() {
                        self.canvas_mode = m;
                    }
                }
                ui.label("canvas:");
            });
        });
    }

    /// URL import row (shared box, two deck targets). Lives in the Advanced drawer.
    fn url_import_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("🔗 URL");
            ui.add(
                egui::TextEdit::singleline(&mut self.import_url)
                    .hint_text("paste a video/audio URL")
                    .desired_width(360.0),
            );
            let has_url = !self.import_url.trim().is_empty();
            for d in 0..2 {
                let label = if d == 0 { "⬇A" } else { "⬇B" };
                let can = self.ytdlp_ok && has_url && self.busy[d].is_none();
                let btn = ui.add_enabled(can, egui::Button::new(label));
                let btn = if self.ytdlp_ok {
                    btn.on_hover_text(if d == 0 { "import → Deck A" } else { "import → Deck B" })
                } else {
                    btn.on_disabled_hover_text("install yt-dlp: uv tool install yt-dlp")
                };
                if btn.clicked() {
                    self.start_import(d);
                }
            }
        });
    }

    /// Shared mic / aux gain + cue note. Lives in the Advanced drawer.
    fn shared_gains(&mut self, ui: &mut egui::Ui) {
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
        ui.weak("🎧 cue needs a 4+ output device (Arrow out 3/4); ignored on a 2ch output.");
    }

    /// Ableton Link panel. Lives in the Advanced drawer.
    fn link_panel(&mut self, ui: &mut egui::Ui) {
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

        // Collect finished image-dialog picks, per deck → static-image source.
        for d in 0..2 {
            if let Some(rx) = &self.image_rx[d] {
                if let Ok(result) = rx.try_recv() {
                    if let Some(path) = result {
                        self.load_image(d, path);
                    }
                    self.image_rx[d] = None;
                }
            }
        }

        // Collect finished URL downloads, per deck → load the mp4 (audio+video).
        for d in 0..2 {
            if let Some(rx) = &self.import_rx[d] {
                if let Ok(result) = rx.try_recv() {
                    match result {
                        Ok(path) => self.load_path(d, path), // sets deck_path too.
                        Err(e) => eprintln!("import (deck {d}) failed: {e}"),
                    }
                    self.import_rx[d] = None;
                    self.busy[d] = None;
                }
            }
        }

        // Collect finished karaoke splits, per deck → swap AUDIO to the
        // instrumental stem (video untouched). Keep deck_path on the ORIGINAL so a
        // re-split still works. load_track_background resets the playhead to 0 and
        // pauses — the stem shares the original's timeline, so that is harmless.
        for d in 0..2 {
            if let Some(rx) = &self.karaoke_rx[d] {
                if let Ok(result) = rx.try_recv() {
                    match result {
                        Ok(instrumental) => {
                            self.engine.load_track_background(d, instrumental);
                        }
                        Err(e) => eprintln!("karaoke (deck {d}) failed: {e}"),
                    }
                    self.karaoke_rx[d] = None;
                    self.busy[d] = None;
                }
            }
        }

        // Pull the latest video frame into each deck's GPU texture, but only when
        // its worker reports a new generation (no per-repaint re-upload).
        for d in 0..2 {
            if let Some(source) = &self.video[d] {
                let slot = source.slot();
                let gen = slot.generation();
                if gen != self.last_gen[d] {
                    let img = slot.with_latest(|fr| {
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
            // 1. TOP BAR: title · BPM match lamp · canvas · Output · Snap.
            self.top_bar(ui);
            ui.add_space(6.0);

            // 2. STACKED A/B OVERVIEW — the primary beat-match view (blue over
            // red). Click the top half to seek A, the bottom half to seek B.
            self.draw_stacked_overview(ui, 110.0);
            ui.add_space(8.0);

            // 3. VIDEO COMPOSITE (hero): deck A under deck B, blended by the
            // crossfader. Canvas-aware, ~300 px cap.
            self.video_composite(ui);

            // 4. THREE COLUMNS: DECK A | MIXER | DECK B. Manual split — decks
            // flex, the mixer is a fixed narrow column.
            let full_w = ui.available_width();
            let spacing = ui.spacing().item_spacing.x;
            let deck_w = ((full_w - MIXER_W - 2.0 * spacing) * 0.5).max(120.0);
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(deck_w, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(deck_w);
                        self.deck_panel(ui, 0);
                    },
                );
                ui.separator();
                ui.allocate_ui_with_layout(
                    egui::vec2(MIXER_W, 0.0),
                    egui::Layout::top_down(egui::Align::Center),
                    |ui| {
                        ui.set_width(MIXER_W);
                        self.mixer_panel(ui, 130.0);
                    },
                );
                ui.separator();
                ui.allocate_ui_with_layout(
                    egui::vec2(deck_w, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(deck_w);
                        self.deck_panel(ui, 1);
                    },
                );
            });

            ui.add_space(6.0);

            // 5. CROSSFADER — full width, framed + big A/B so it reads as THE mix
            // control. Drag it: blends both the audio AND the video composite.
            egui::Frame::group(ui.style())
                .fill(Color32::from_gray(20))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.colored_label(
                            DECK_COLOR[0],
                            egui::RichText::new("◀ A").strong().size(18.0),
                        );
                        // Explicit-size slider (no slider_width hack), value shown so
                        // it's obvious it's moving. Drag blends audio AND video.
                        let w = (ui.available_width() - 64.0).max(80.0);
                        ui.add_sized(
                            [w, 22.0],
                            egui::Slider::new(&mut self.crossfade, 0.0..=1.0).show_value(true),
                        );
                        // Push every frame (cheap atomic) so the mix always tracks it.
                        self.engine.set_crossfade(self.crossfade);
                        ui.colored_label(
                            DECK_COLOR[1],
                            egui::RichText::new("B ▶").strong().size(18.0),
                        );
                    });
                });

            ui.add_space(8.0);

            // 6. ADVANCED — open by default so nothing looks "removed"; collapse it
            // for a clean set view. Holds Master FX, URL import, mic/aux, Link.
            egui::CollapsingHeader::new("⚙ Advanced — Master FX · Import · Mic/Aux · Link")
                .default_open(true)
                .show(ui, |ui| {
                    self.master_fx_panel(ui);
                    ui.separator();
                    self.url_import_row(ui);
                    ui.separator();
                    self.shared_gains(ui);
                    ui.separator();
                    self.link_panel(ui);
                    ui.separator();
                    ui.label(format!(
                        "device {} Hz · underruns {}",
                        self.engine.device_sr(),
                        self.engine.underruns()
                    ));
                });
        });

        // Standalone OUTPUT WINDOW: a separate OS window showing ONLY the live
        // composite (no controls). Fullscreen it on a second display and let OBS
        // grab it via Window/Display Capture. The texture upload above already
        // ran this frame, so we just re-paint the SAME shared textures here.
        if self.output_open {
            let video_tex = &self.video_tex;
            let crossfade = self.crossfade;
            let canvas_aspect = self.canvas_mode.aspect(&self.video_tex);
            let mut close_requested = false;
            ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of("livehub_output"),
                egui::ViewportBuilder::default()
                    .with_title("livehub output")
                    .with_inner_size([1280.0, 720.0]),
                |vctx, _class| {
                    egui::CentralPanel::default()
                        .frame(egui::Frame::NONE.fill(Color32::BLACK))
                        .show(vctx, |ui| {
                            // No height cap → fill the viewport, black letterbox.
                            App::paint_composite_into(
                                ui,
                                video_tex,
                                crossfade,
                                canvas_aspect,
                                None,
                            );
                        });
                    // OS close button (red dot / ⌘W): note it, clear flag below.
                    if vctx.input(|i| i.viewport().close_requested()) {
                        close_requested = true;
                    }
                    vctx.request_repaint();
                },
            );
            // Closed via the window's own button → don't reopen next frame.
            if close_requested {
                self.output_open = false;
            }
        }

        // Keep the playhead/meters live.
        ctx.request_repaint();
    }
}

/// Largest rect of the given width:height `aspect` centered inside `outer`
/// (letterbox/pillarbox). Used to fit the 16:9 stage in its row and each clip
/// into the stage at its own shape — so nothing is ever stretched.
fn fit_aspect(outer: Rect, aspect: f32) -> Rect {
    let (mut w, mut h) = (outer.width(), outer.width() / aspect.max(0.0001));
    if h > outer.height() {
        h = outer.height();
        w = outer.height() * aspect;
    }
    Rect::from_center_size(outer.center(), egui::vec2(w, h))
}

/// Round `t` (seconds) to the nearest beat time. Returns `t` unchanged when the
/// beatgrid is empty.
fn snap_to_beat(beats: &[f64], t: f64) -> f64 {
    beats
        .iter()
        .copied()
        .min_by(|a, b| {
            (a - t).abs().partial_cmp(&(b - t).abs()).unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(t)
}

/// Draw ONE deck's lane of the stacked overview into `lane`: peak envelope
/// (tinted `accent`), an engaged-loop band, beat ticks (downbeats brighter),
/// and a moving playhead. Pure painting — interaction (click-to-seek) is owned
/// by the caller, which knows which half maps to which deck. No-ops on
/// empty/zero-duration tracks.
fn draw_overview_lane(
    painter: &egui::Painter,
    lane: Rect,
    track: Option<&TrackBuf>,
    duration: f64,
    playhead: f64,
    loop_region: Option<(f64, f64)>,
    accent: Color32,
) {
    let map_x = |nx: f32| lane.left() + nx.clamp(0.0, 1.0) * lane.width();

    let Some(track) = track else { return };
    let dur = duration.max(f64::MIN_POSITIVE);

    // Reserve a thin beat lane at the bottom; the waveform owns the area above.
    let lane_h = 10.0_f32;
    let wave_bottom = lane.bottom() - lane_h;
    let wave_mid = (lane.top() + wave_bottom) * 0.5;
    let wave_half = (wave_bottom - lane.top()) * 0.5;
    let map_y = |v: f32| wave_mid - v.clamp(-1.0, 1.0) * wave_half;

    // Engaged beat-loop band, under the waveform but over the background.
    if let Some((ls, le)) = loop_region {
        let band = Rect::from_min_max(
            Pos2::new(map_x((ls / dur) as f32), lane.top()),
            Pos2::new(map_x((le / dur) as f32), wave_bottom),
        );
        painter.rect_filled(band, 0.0, Color32::from_rgba_unmultiplied(255, 210, 40, 40));
    }

    // Peak envelope, tinted with the deck accent.
    let wave_stroke = Stroke::new(1.0, accent);
    let n = track.waveform.len().max(1);
    for (i, &(lo, hi)) in track.waveform.iter().enumerate() {
        let x = map_x(i as f32 / n as f32);
        painter.line_segment([Pos2::new(x, map_y(hi)), Pos2::new(x, map_y(lo))], wave_stroke);
    }

    // Beat ruler: short ticks per beat, taller brighter ticks on downbeats.
    let beat_stroke = Stroke::new(1.0, Color32::from_gray(110));
    let beat_top = lane.bottom() - 4.0;
    for &t in &track.beats {
        let x = map_x((t / dur) as f32);
        painter.line_segment([Pos2::new(x, beat_top), Pos2::new(x, lane.bottom())], beat_stroke);
    }
    let down_stroke = Stroke::new(1.5, Color32::from_rgb(255, 205, 110));
    let down_top = lane.bottom() - lane_h;
    for &t in &track.downbeats {
        let x = map_x((t / dur) as f32);
        painter.line_segment([Pos2::new(x, down_top), Pos2::new(x, lane.bottom())], down_stroke);
    }

    // Playhead cursor on top (full lane height).
    let px = map_x((playhead / dur) as f32);
    painter.line_segment(
        [Pos2::new(px, lane.top()), Pos2::new(px, lane.bottom())],
        Stroke::new(2.0, Color32::WHITE),
    );
}
