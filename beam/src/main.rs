//! **beam** — a tiny standalone window that captures an input device (BlackHole
//! carrying REAPER, system audio, a hardware input) and UDP-streams it to the
//! phone (reelcam) over WiFi. The "just send audio, no DJ console" tool split out
//! of livehub. Reuses `livehub-engine`'s `Broadcaster` + `Discovery`.
//!
//! A small always-on-top window (not a menu-bar item — a full menu bar can hide
//! those): pick Source + Phone (auto-discovered), hit ▶. Closing the window quits
//! (and stops the stream); ⌘H tucks it away while it keeps broadcasting.

mod gallery;
mod multipeer;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use livehub_engine::{
    input_device_names, load_last_phone, save_last_phone, Broadcaster, Discovery, LevelMonitor, Peer,
    PreviewReceiver, RemoteControl, CAM_AE_LOCK, CAM_EV, CAM_FLIP, CAM_ISO, CAM_LENS,
    CAM_LENS_SELECT, CAM_MANUAL_EXP, CAM_MANUAL_FOCUS, CAM_SHUTTER, CMD_PREVIEW_START,
    CMD_PREVIEW_STOP, CMD_REC_TOGGLE,
};

fn main() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 560.0])
            .with_min_inner_size([520.0, 360.0])
            .with_always_on_top()
            .with_title("beam"),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "beam",
        options,
        Box::new(|_cc| Ok(Box::new(Beam::new()))),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

struct Beam {
    devices: Vec<String>,
    sel_device: String,
    sample_rate: u32, // broadcast rate; the phone follows it from the packet header
    discovery: Option<Discovery>,
    peers: Vec<Peer>,
    phone_addr: String, // ip:port text (editable + filled by discovery)
    remembered: Option<String>,
    broadcaster: Option<Broadcaster>,
    status: String,
    // Remote control of the phone + its camera preview (MJPEG over UDP).
    remote_rec: bool, // local toggle intent (the phone has no ack channel)
    preview: Option<PreviewReceiver>,
    preview_tex: Option<egui::TextureHandle>,
    preview_seq: u32, // last frame seq we uploaded → detect new frames
    last_heartbeat: Instant, // re-arm the phone's preview watchdog
    control: Option<RemoteControl>, // persistent control socket (stable port)
    // A/V-sync test: periodically flash the window + click the audio together.
    sync_test: bool,
    last_pulse: Instant,
    flash: bool,
    // Remote camera controls (beam → phone) + a luma histogram of the preview.
    // Values are normalized 0..1; the phone maps them onto its device ranges.
    histogram: [u32; 64],
    // Preview frame-rate meter (diagnoses latency: high fps + visible lag = WiFi
    // bufferbloat; low fps = uplink/encode bound).
    preview_frames: u32,
    preview_fps: f32,
    preview_fps_at: Instant,
    cam_manual_exp: bool,
    cam_iso: f32,
    cam_shutter: f32,
    cam_ev: f32, // 0.5 = 0 EV
    cam_manual_focus: bool,
    cam_lens: f32,
    cam_ae_lock: bool,
    cam_front: bool, // tracked flip intent (the phone has no ack channel)
    // Pre-broadcast input level meter (so you can SEE signal before hitting ▶).
    level_monitor: Option<LevelMonitor>,
    monitored_device: String,
    // Remote gallery: browse + pull the phone's recorded clips over WiFi.
    gallery: gallery::Gallery,
    tab: Tab,
    // Multipeer (AWDL) transport — Phase 1: connect + round-trip test, no router/SIM.
    multipeer: multipeer::Multipeer,
    mp_last_recv: String,
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Preview,
    Gallery,
}

impl Beam {
    fn new() -> Self {
        let (remembered, saved_addr) = load_last_phone();
        let devices = input_device_names();
        // Default to a BlackHole-ish device if present (the usual REAPER loopback).
        let sel_device = devices
            .iter()
            .find(|d| d.to_lowercase().contains("blackhole"))
            .cloned()
            .or_else(|| devices.first().cloned())
            .unwrap_or_default();
        Beam {
            devices,
            sel_device,
            sample_rate: 0, // 0 = Auto (use the source device's own rate)
            discovery: Discovery::start().ok(),
            peers: Vec::new(),
            phone_addr: saved_addr,
            remembered,
            broadcaster: None,
            status: String::new(),
            remote_rec: false,
            preview: None,
            preview_tex: None,
            preview_seq: 0,
            last_heartbeat: Instant::now(),
            control: RemoteControl::new().ok(),
            sync_test: false,
            last_pulse: Instant::now(),
            flash: false,
            histogram: [0; 64],
            preview_frames: 0,
            preview_fps: 0.0,
            preview_fps_at: Instant::now(),
            cam_manual_exp: false,
            cam_iso: 0.3,
            cam_shutter: 0.3,
            cam_ev: 0.5,
            cam_manual_focus: false,
            cam_lens: 0.5,
            cam_ae_lock: false,
            cam_front: false,
            level_monitor: None,
            monitored_device: String::new(),
            gallery: gallery::Gallery::default(),
            tab: Tab::Preview,
            multipeer: multipeer::Multipeer::default(),
            mp_last_recv: String::new(),
        }
    }

    /// The phone's IP (without the audio port) — the gallery's file server is on its
    /// own port on the same host.
    fn phone_ip(&self) -> Option<String> {
        self.phone_addr.parse::<SocketAddr>().ok().map(|a| a.ip().to_string())
    }

    /// The remote-gallery window: refresh, thumbnail grid, per-clip download.
    fn gallery_ui(&mut self, ui: &mut egui::Ui) {
        let host = self.phone_ip();
        ui.horizontal(|ui| {
            if ui.add_enabled(host.is_some(), egui::Button::new("🔄 refresh")).clicked() {
                if let Some(h) = host.clone() {
                    self.gallery.refresh(h);
                }
            }
            if self.gallery.busy {
                ui.spinner();
            }
            ui.label(self.gallery.status.clone());
        });
        ui.horizontal(|ui| {
            ui.weak(format!("downloads → {}", self.gallery.dir.display()));
            if ui.small_button("📂 open").clicked() {
                gallery::open_path(&self.gallery.dir);
            }
            if ui.small_button("🗑 Mac folder").clicked() {
                // Wipe the local download folder on the Mac.
                if let Ok(entries) = std::fs::read_dir(&self.gallery.dir) {
                    for e in entries.flatten() {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
            if ui.small_button("🗑 phone (all)").clicked() {
                // Delete ALL clips from the phone's Photos — one confirmation on the device.
                if let Some(h) = host.clone() {
                    self.gallery.delete_all_remote(h);
                }
            }
        });
        ui.separator();
        let items = self.gallery.items.clone();
        if items.is_empty() {
            ui.weak("No clips — hit refresh (turn the gallery ON in Reelcam settings first).");
            return;
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("clips").spacing(egui::vec2(10.0, 10.0)).show(ui, |ui| {
                for (i, clip) in items.iter().enumerate() {
                    ui.vertical(|ui| {
                        let tid = self.gallery.thumbs.get(&clip.name).map(|t| t.id());
                        if let Some(id) = tid {
                            ui.add(egui::Image::new(egui::load::SizedTexture::new(
                                id,
                                egui::vec2(130.0, 130.0),
                            )));
                        } else {
                            let (r, _) = ui.allocate_exact_size(
                                egui::vec2(130.0, 130.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().rect_filled(r, 4.0, egui::Color32::from_gray(30));
                        }
                        let mb = clip.size / 1_000_000;
                        let size = if mb > 0 { format!(" · {mb} MB") } else { String::new() };
                        ui.label(format!("{} · {:.0}s{}", clip.date, clip.duration, size));
                        // Dedup by filename: if it's already downloaded, play the local
                        // copy instead of re-downloading.
                        let local = self.gallery.dir.join(&clip.name);
                        let have = local.exists();
                        ui.horizontal(|ui| {
                            if have {
                                if ui.button("▶ play").clicked() {
                                    gallery::open_path(&local);
                                }
                                ui.colored_label(egui::Color32::from_rgb(90, 200, 110), "✓");
                            } else if ui.button("↓ download").clicked() {
                                if let Some(h) = host.clone() {
                                    self.gallery.download(h, clip.name.clone());
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            if have && ui.small_button("🗑 mac").clicked() {
                                let _ = std::fs::remove_file(&local);
                            }
                            if ui.small_button("🗑 phone").clicked() {
                                if let Some(h) = host.clone() {
                                    self.gallery.delete_remote(h, clip.name.clone());
                                }
                            }
                        });
                    });
                    if (i + 1) % 3 == 0 {
                        ui.end_row();
                    }
                }
            });
        });
    }

    /// Keep a pre-broadcast level meter running on the selected device while NOT
    /// broadcasting (so you can confirm signal before ▶). Restart it if the device
    /// changed; drop it while broadcasting (the broadcaster owns the device then).
    fn update_level_monitor(&mut self) {
        if self.on() {
            self.level_monitor = None; // broadcaster provides the level
            self.monitored_device.clear();
            return;
        }
        if self.sel_device.is_empty() {
            self.level_monitor = None;
            return;
        }
        if self.level_monitor.is_none() || self.monitored_device != self.sel_device {
            self.level_monitor = LevelMonitor::start(&self.sel_device).ok();
            self.monitored_device = self.sel_device.clone();
        }
    }

    /// Send a camera parameter (normalized 0..1) to the phone over the persistent
    /// control socket (same stable port as the other commands).
    fn cam(&self, param: u8, value: f32) {
        if let (Some(addr), Some(c)) = (self.phone(), self.control.as_ref()) {
            let _ = c.send_cam(addr, param, value);
        }
    }

    /// Send a control command to the phone over the persistent socket (stable
    /// source port = one connection on the phone; no flow churn). Returns whether
    /// the datagram went out.
    fn ctl(&self, to: SocketAddr, cmd: u8, port: u16) -> bool {
        self.control.as_ref().is_some_and(|c| c.send(to, cmd, port).is_ok())
    }

    fn on(&self) -> bool {
        self.broadcaster.is_some()
    }

    /// The phone's control endpoint (ip:port parsed from the target field).
    fn phone(&self) -> Option<SocketAddr> {
        self.phone_addr.parse::<SocketAddr>().ok()
    }

    /// Toggle recording ON THE PHONE (no ack — we track intent locally).
    fn toggle_remote_record(&mut self) {
        if let Some(addr) = self.phone() {
            if self.ctl(addr, CMD_REC_TOGGLE, 0) {
                self.remote_rec = !self.remote_rec;
            }
        }
    }

    /// Start/stop receiving the phone's camera preview (MJPEG → one texture).
    fn toggle_preview(&mut self) {
        if self.preview.is_some() {
            if let Some(addr) = self.phone() {
                self.ctl(addr, CMD_PREVIEW_STOP, 0);
            }
            self.preview = None;
            self.preview_tex = None; // drop the texture → free GPU memory
            return;
        }
        // Start a receiver on an ephemeral port, tell the phone to send there.
        if let (Some(addr), Ok(rx)) = (self.phone(), PreviewReceiver::start()) {
            self.ctl(addr, CMD_PREVIEW_START, rx.port());
            self.preview = Some(rx);
        }
    }

    /// If a newer preview frame arrived, decode it into `preview_tex`. Keeps only
    /// the latest frame — nothing is buffered or stored.
    fn poll_preview(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.preview else { return };
        let seq = rx.frame_seq();
        if seq == self.preview_seq {
            return;
        }
        self.preview_seq = seq;
        // FPS meter: count displayed frames, recompute once a second.
        self.preview_frames += 1;
        let dt = self.preview_fps_at.elapsed().as_secs_f32();
        if dt >= 1.0 {
            self.preview_fps = self.preview_frames as f32 / dt;
            self.preview_frames = 0;
            self.preview_fps_at = Instant::now();
        }
        let Some(jpeg) = rx.latest_jpeg() else { return };
        if let Ok(img) = image::load_from_memory(&jpeg) {
            let rgba = img.to_rgba8();
            // Luma histogram (BT.709) of the frame, subsampled every 4th pixel.
            let mut hist = [0u32; 64];
            let raw = rgba.as_raw();
            let mut i = 0;
            while i + 2 < raw.len() {
                let luma = (raw[i] as u32 * 54 + raw[i + 1] as u32 * 183 + raw[i + 2] as u32 * 19) >> 8;
                hist[(luma.min(255) as usize) >> 2] += 1;
                i += 16; // RGBA = 4 bytes → every 4th pixel
            }
            self.histogram = hist;
            let (w, h) = (rgba.width() as usize, rgba.height() as usize);
            let color = egui::ColorImage::from_rgba_unmultiplied([w, h], rgba.as_raw());
            // Replace in place — the old texture is freed, so memory stays bounded.
            match &mut self.preview_tex {
                Some(tex) => tex.set(color, egui::TextureOptions::LINEAR),
                None => {
                    self.preview_tex =
                        Some(ctx.load_texture("preview", color, egui::TextureOptions::LINEAR))
                }
            }
        }
    }

    /// Camera remote + histogram, drawn under the preview. Each control change sends
    /// a normalized cam-param to the phone; the phone maps it to its device ranges.
    fn camera_controls_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(format!("📊 {:.0} fps", self.preview_fps));
            self.draw_histogram(ui);
        });
        // Phone camera selection — switchable live (lens 0.5×/1×/tele + front/back).
        ui.horizontal(|ui| {
            ui.label("Cam:");
            if ui.button("0.5×").clicked() {
                self.cam(CAM_LENS_SELECT, 0.0);
            }
            if ui.button("1×").clicked() {
                self.cam(CAM_LENS_SELECT, 1.0);
            }
            if ui.button("Tele").clicked() {
                self.cam(CAM_LENS_SELECT, 2.0);
            }
            ui.separator();
            let flip = if self.cam_front { "⟳ front" } else { "⟳ rear" };
            if ui.button(flip).clicked() {
                self.cam_front = !self.cam_front;
                self.cam(CAM_FLIP, if self.cam_front { 1.0 } else { 0.0 });
            }
        });
        ui.horizontal_wrapped(|ui| {
            if ui.selectable_label(self.cam_ae_lock, "🔒 AE").clicked() {
                self.cam_ae_lock = !self.cam_ae_lock;
                self.cam(CAM_AE_LOCK, if self.cam_ae_lock { 1.0 } else { 0.0 });
            }
            if ui.checkbox(&mut self.cam_manual_exp, "Manual exp").changed() {
                self.cam(CAM_MANUAL_EXP, if self.cam_manual_exp { 1.0 } else { 0.0 });
            }
            if ui.checkbox(&mut self.cam_manual_focus, "Manual focus").changed() {
                self.cam(CAM_MANUAL_FOCUS, if self.cam_manual_focus { 1.0 } else { 0.0 });
            }
        });
        if self.cam_manual_exp {
            if ui.add(egui::Slider::new(&mut self.cam_iso, 0.0..=1.0).text("ISO")).changed() {
                self.cam(CAM_ISO, self.cam_iso);
            }
            if ui.add(egui::Slider::new(&mut self.cam_shutter, 0.0..=1.0).text("Shutter")).changed() {
                self.cam(CAM_SHUTTER, self.cam_shutter);
            }
        } else {
            let ev = egui::Slider::new(&mut self.cam_ev, 0.0..=1.0)
                .text("EV")
                .custom_formatter(|v, _| format!("{:+.1}", v * 6.0 - 3.0));
            if ui.add(ev).changed() {
                self.cam(CAM_EV, self.cam_ev);
            }
        }
        if self.cam_manual_focus
            && ui.add(egui::Slider::new(&mut self.cam_lens, 0.0..=1.0).text("Focus")).changed()
        {
            self.cam(CAM_LENS, self.cam_lens);
        }
    }

    /// Draw the luma histogram (64 bins) as a small bar strip.
    fn draw_histogram(&self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(200.0, 42.0), egui::Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 2.0, egui::Color32::from_gray(28));
        let max = self.histogram.iter().copied().max().unwrap_or(1).max(1) as f32;
        let bw = rect.width() / self.histogram.len() as f32;
        for (i, &c) in self.histogram.iter().enumerate() {
            let bh = (c as f32 / max) * rect.height();
            let x = rect.left() + i as f32 * bw;
            p.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(x, rect.bottom() - bh),
                    egui::pos2(x + bw.max(1.0), rect.bottom()),
                ),
                0.0,
                egui::Color32::from_gray(210),
            );
        }
    }

    fn start(&mut self) {
        self.level_monitor = None; // release the device before the broadcaster opens it
        match self.phone_addr.parse::<SocketAddr>() {
            Ok(addr) => match Broadcaster::start(&self.sel_device, addr, self.sample_rate) {
                Ok(b) => {
                    self.broadcaster = Some(b);
                    self.status = format!("streaming → {addr}");
                    if let Some(name) =
                        self.peers.iter().find(|p| p.addr == addr).map(|p| p.name.clone())
                    {
                        self.remembered = Some(name.clone());
                        save_last_phone(&name, &addr.to_string());
                    } else {
                        save_last_phone("", &addr.to_string());
                    }
                }
                Err(e) => self.status = format!("error: {e}"),
            },
            Err(_) => self.status = "bad ip:port (e.g. 172.20.10.2:9000)".into(),
        }
    }
}

impl eframe::App for Beam {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Refresh discovered phones; auto-fill the remembered one (even on a new
        // IP) or the only one present, while not broadcasting.
        if let Some(d) = &self.discovery {
            self.peers = d.peers();
        }
        if !self.on() {
            if let Some(name) = &self.remembered {
                if let Some(p) = self.peers.iter().find(|p| &p.name == name) {
                    self.phone_addr = p.addr.to_string();
                }
            }
            if self.phone_addr.is_empty() && self.peers.len() == 1 {
                self.phone_addr = self.peers[0].addr.to_string();
            }
        }

        // Keep the pre-broadcast level meter alive on the selected device.
        self.update_level_monitor();

        // Pull the latest camera-preview frame (if any) into the texture.
        self.poll_preview(ctx);
        // Drain remote-gallery background results (list / thumbs / downloads).
        self.gallery.poll(ctx);
        // Drain Multipeer frames (Phase 1: show the last as text for the round-trip test).
        for f in self.multipeer.drain() {
            self.mp_last_recv = String::from_utf8_lossy(&f).chars().take(60).collect();
        }

        // Heartbeat: while previewing, re-arm the phone's watchdog (~every 2 s) so
        // it keeps streaming; if we crash, it self-stops after the timeout.
        if self.preview.is_some() && self.last_heartbeat.elapsed() >= Duration::from_secs(2) {
            self.last_heartbeat = Instant::now();
            let port = self.preview.as_ref().map(|r| r.port());
            if let (Some(addr), Some(port)) = (self.phone(), port) {
                self.ctl(addr, CMD_PREVIEW_START, port);
            }
        }

        // A/V-sync pulse: every ~1.5 s flash the window AND click the audio at the
        // same instant. Film this window + record the audio, then analyse the clip:
        // the flash↔click offset is the audio-vs-video latency. The flash is HELD
        // ~80 ms (a few frames) so a 30 fps camera reliably catches a fully-white
        // frame — the click still fires once, on the rising edge (what we measure).
        if self.sync_test && self.on() && self.last_pulse.elapsed() >= Duration::from_millis(1500) {
            self.last_pulse = Instant::now();
            if let Some(b) = &self.broadcaster {
                b.pulse();
            }
        }
        self.flash = self.sync_test && self.on()
            && self.last_pulse.elapsed() < Duration::from_millis(80);

        // ── LEFT: controls ────────────────────────────────────────────────────
        egui::SidePanel::left("controls")
            .resizable(false)
            .exact_width(340.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(4.0);
                    ui.heading("📡 beam");
                    ui.separator();

                    // Discovered phones.
                    if self.peers.is_empty() {
                        ui.weak("📱 searching for reelcam…");
                    } else {
                        ui.label("📱 phones (click to target):");
                        for p in &self.peers {
                            let star = if self.remembered.as_deref() == Some(p.name.as_str()) {
                                "⭐ "
                            } else {
                                ""
                            };
                            if ui.button(format!("{star}{} · {}", p.name, p.addr)).clicked() {
                                self.phone_addr = p.addr.to_string();
                                self.remembered = Some(p.name.clone());
                                save_last_phone(&p.name, &self.phone_addr);
                            }
                        }
                    }
                    ui.add_space(6.0);

                    // Source device.
                    ui.label("Source device:");
                    egui::ComboBox::from_id_salt("dev")
                        .selected_text(if self.sel_device.is_empty() {
                            "— pick —"
                        } else {
                            self.sel_device.as_str()
                        })
                        .width(300.0)
                        .show_ui(ui, |ui| {
                            for d in &self.devices {
                                ui.selectable_value(&mut self.sel_device, d.clone(), d.as_str());
                            }
                        });
                    ui.add_space(4.0);
                    // Broadcast sample rate. The phone follows it automatically (it's
                    // in every audio packet header), so this is the single source of
                    // truth — no separate setting on the phone. Locked while streaming.
                    ui.label("Sample rate (phone follows):");
                    let rate_label =
                        |sr: u32| if sr == 0 { "Auto (device)".to_owned() } else { format!("{sr} Hz") };
                    ui.add_enabled_ui(!self.on(), |ui| {
                        egui::ComboBox::from_id_salt("sr")
                            .selected_text(rate_label(self.sample_rate))
                            .width(300.0)
                            .show_ui(ui, |ui| {
                                for sr in [0u32, 44_100, 48_000, 88_200, 96_000] {
                                    ui.selectable_value(&mut self.sample_rate, sr, rate_label(sr));
                                }
                            });
                    });
                    if let Some(b) = &self.broadcaster {
                        ui.weak(format!("⏵ streaming at {} Hz · ⏹ stop to change", b.sample_rate()));
                    } else {
                        ui.weak("Auto = whatever the source device runs at");
                    }
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label("Phone (ip:port):");
                        if ui
                            .add_enabled(!self.on(), egui::Button::new("🔄 refresh"))
                            .on_hover_text("re-scan the network for the phone (if it dropped off)")
                            .clicked()
                        {
                            if let Some(d) = &mut self.discovery {
                                d.refresh();
                            }
                            self.peers.clear(); // auto-refill updates phone_addr when it reappears
                            self.status = "re-scanning…".into();
                        }
                    });
                    ui.add_enabled(
                        !self.on(),
                        egui::TextEdit::singleline(&mut self.phone_addr)
                            .hint_text("172.20.10.2:9000")
                            .desired_width(300.0),
                    );

                    // BIG broadcast button.
                    ui.add_space(8.0);
                    let bl = if self.on() { "■ stop broadcast" } else { "▶ broadcast" };
                    if ui
                        .add(egui::Button::new(egui::RichText::new(bl).size(16.0))
                            .min_size(egui::vec2(300.0, 42.0)))
                        .clicked()
                    {
                        if self.on() {
                            self.broadcaster = None;
                            self.status = "stopped".into();
                        } else {
                            self.start();
                        }
                    }
                    // Level: from the broadcaster while streaming, else the pre-stream
                    // monitor — so the meter moves BEFORE you hit ▶.
                    let level = if let Some(b) = &self.broadcaster {
                        b.level()
                    } else {
                        self.level_monitor.as_ref().map(|m| m.level()).unwrap_or(0.0)
                    };
                    ui.add_space(4.0);
                    level_bar(ui, level, self.on() || self.level_monitor.is_some());
                    if !self.status.is_empty() {
                        ui.weak(&self.status);
                    }
                    if self.on() && level < 0.001 {
                        ui.colored_label(
                            egui::Color32::from_rgb(230, 170, 70),
                            "⚠ source silent — is signal reaching it?",
                        );
                    }

                    // BIG phone-remote buttons.
                    ui.add_space(8.0);
                    ui.separator();
                    ui.label("📲 Phone remote:");
                    let has_phone = self.phone().is_some();
                    let rec = if self.remote_rec { "■ stop rec" } else { "⏺ record" };
                    let rec_btn = egui::Button::new(egui::RichText::new(rec).size(16.0))
                        .min_size(egui::vec2(300.0, 42.0))
                        .fill(if self.remote_rec {
                            egui::Color32::from_rgb(120, 40, 40)
                        } else {
                            egui::Color32::from_gray(52)
                        });
                    if ui.add_enabled(has_phone, rec_btn).clicked() {
                        self.toggle_remote_record();
                    }
                    ui.add_space(4.0);
                    let prev = if self.preview.is_some() { "👁 preview: ON" } else { "👁 preview: off" };
                    if ui
                        .add_enabled(
                            has_phone,
                            egui::Button::new(prev).min_size(egui::vec2(300.0, 34.0)),
                        )
                        .clicked()
                    {
                        self.toggle_preview();
                    }

                    // Remote gallery — browse + pull recorded clips over WiFi.
                    ui.add_space(4.0);
                    if ui
                        .add_enabled(
                            has_phone,
                            egui::Button::new("📁 gallery").min_size(egui::vec2(300.0, 34.0)),
                        )
                        .clicked()
                    {
                        self.tab = Tab::Gallery;
                        if let Some(h) = self.phone_ip() {
                            self.gallery.refresh(h);
                        }
                    }

                    // A/V-sync test pulse (needs broadcast running).
                    ui.add_space(4.0);
                    let st = if self.sync_test { "🎬 sync test: ON" } else { "🎬 sync test" };
                    if ui
                        .add_enabled(
                            self.on(),
                            egui::Button::new(st).min_size(egui::vec2(300.0, 34.0)),
                        )
                        .clicked()
                    {
                        self.sync_test = !self.sync_test;
                        // Mute the device audio during the test so only the click is
                        // heard — music/voice would pollute click detection.
                        if let Some(b) = &self.broadcaster {
                            b.set_silent(self.sync_test);
                        }
                    }
                    if self.sync_test {
                        ui.weak("muted except the click · film THIS window · then avsync.py");
                    }

                    // ── Multipeer (AWDL) — no router / SIM / internet (Phase 1) ──
                    ui.add_space(8.0);
                    ui.separator();
                    ui.label("🔗 Multipeer link (no-WiFi):");
                    let mp_on = self.multipeer.running();
                    let lbl = if !mp_on {
                        "▶ start link"
                    } else if self.multipeer.connected() {
                        "● connected"
                    } else {
                        "… searching"
                    };
                    if ui
                        .add(egui::Button::new(lbl).min_size(egui::vec2(300.0, 30.0)).fill(
                            if self.multipeer.connected() {
                                egui::Color32::from_rgb(30, 90, 50)
                            } else {
                                egui::Color32::from_gray(52)
                            },
                        ))
                        .clicked()
                    {
                        if mp_on {
                            self.multipeer.stop();
                        } else {
                            self.multipeer.start();
                        }
                    }
                    if mp_on {
                        if ui.add_enabled(self.multipeer.connected(), egui::Button::new("ping")).clicked()
                        {
                            self.multipeer.send(b"ping".to_vec());
                        }
                        ui.weak(format!("{}", self.multipeer.status()));
                        if !self.mp_last_recv.is_empty() {
                            ui.weak(format!("recv: {}", self.mp_last_recv));
                        }
                        ui.weak("turn on 'Multipeer' in Reelcam settings to pair");
                    }

                    ui.add_space(8.0);
                    ui.separator();
                    ui.weak(
                        "🔊 System/YouTube audio needs BlackHole: Audio MIDI Setup → a \
                         Multi-Output (BlackHole + speakers) → set as output, then pick \
                         BlackHole above.",
                    );
                });
            });

        // ── RIGHT: big camera preview ─────────────────────────────────────────
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(egui::Color32::from_gray(14)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tab, Tab::Preview, "👁 Preview");
                    let g = ui.selectable_value(&mut self.tab, Tab::Gallery, "📁 Gallery");
                    if g.clicked() && self.gallery.items.is_empty() {
                        if let Some(h) = self.phone_ip() {
                            self.gallery.refresh(h);
                        }
                    }
                });
                ui.separator();
                match self.tab {
                    Tab::Gallery => self.gallery_ui(ui),
                    Tab::Preview => {
                        // Camera remote + histogram pinned under the preview.
                        if self.preview.is_some() {
                            egui::TopBottomPanel::bottom("cam_ctrls")
                                .resizable(false)
                                .frame(
                                    egui::Frame::default()
                                        .fill(egui::Color32::from_gray(20))
                                        .inner_margin(8.0),
                                )
                                .show_inside(ui, |ui| self.camera_controls_ui(ui));
                        }
                        if let Some(tex) = &self.preview_tex {
                            let avail = ui.available_size();
                            let sz = tex.size_vec2();
                            let aspect = if sz.y > 0.0 { sz.x / sz.y } else { 9.0 / 16.0 };
                            let mut w = avail.x;
                            let mut h = w / aspect;
                            if h > avail.y {
                                h = avail.y;
                                w = h * aspect;
                            }
                            ui.centered_and_justified(|ui| {
                                ui.add(egui::Image::new(egui::load::SizedTexture::new(
                                    tex.id(),
                                    egui::vec2(w, h),
                                )));
                            });
                        } else if self.preview.is_some() {
                            ui.centered_and_justified(|ui| {
                                ui.weak("waiting for preview frames…");
                            });
                        } else {
                            ui.centered_and_justified(|ui| {
                                ui.weak("👁 preview off — turn it on at the left");
                            });
                        }
                    }
                }
            });

        // Full-screen white flash for the A/V-sync test (drawn on top of all panels).
        if self.flash {
            let painter = ctx
                .layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("flash")));
            painter.rect_filled(ctx.content_rect(), 0.0, egui::Color32::WHITE);
        }

        // Repaint cadence: crisp 60 fps during the sync test (well-timed flash),
        // ~6 fps while previewing, else a lazy 2 fps heartbeat.
        let ms = if self.sync_test {
            16
        } else if self.preview.is_some() {
            66 // ~15 fps preview refresh → show the freshest frame sooner
        } else if self.on() || self.level_monitor.is_some() {
            100 // ~10 fps so the audio level meter moves (incl. pre-broadcast)
        } else if self.gallery.open && self.gallery.busy {
            100 // drain gallery downloads/thumbs + animate the spinner
        } else if self.multipeer.running() {
            150 // drain multipeer frames + refresh link status
        } else {
            500
        };
        ctx.request_repaint_after(Duration::from_millis(ms));
    }
}

impl Drop for Beam {
    fn drop(&mut self) {
        // Tell the phone to stop its preview so it doesn't keep sending to a dead
        // port after we quit. (The phone also self-stops on a control timeout.)
        if self.preview.is_some() {
            if let Some(addr) = self.phone() {
                self.ctl(addr, CMD_PREVIEW_STOP, 0);
            }
        }
    }
}

/// Horizontal level meter: green→yellow→red. Dim track when not broadcasting.
fn level_bar(ui: &mut egui::Ui, level: f32, active: bool) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().min(320.0), 14.0),
        egui::Sense::hover(),
    );
    let p = ui.painter();
    p.rect_filled(rect, 3.0, egui::Color32::from_gray(38));
    if active {
        let lvl = level.clamp(0.0, 1.0);
        let color = if lvl > 0.9 {
            egui::Color32::from_rgb(225, 80, 80)
        } else if lvl > 0.6 {
            egui::Color32::from_rgb(230, 200, 70)
        } else {
            egui::Color32::from_rgb(90, 200, 110)
        };
        let fill = egui::Rect::from_min_size(
            rect.min,
            egui::vec2(rect.width() * lvl, rect.height()),
        );
        p.rect_filled(fill, 3.0, color);
    }
}

