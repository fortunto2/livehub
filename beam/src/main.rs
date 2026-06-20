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
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use livehub_engine::{
    input_device_names, load_last_phone, save_last_phone, Broadcaster, Discovery, LevelMonitor,
    MicMonitor, Peer,
    PreviewReceiver, RemoteControl, CAM_AE_LOCK, CAM_EV, CAM_FLIP, CAM_ISO, CAM_LENS,
    camparam_packet, control_packet, CAM_LENS_SELECT, CAM_MANUAL_EXP, CAM_MANUAL_FOCUS,
    CAM_SHUTTER, CMD_PREVIEW_QUALITY, CMD_PREVIEW_START, CMD_PREVIEW_STOP, CMD_REC_TOGGLE,
    PREVIEW_MAGIC,
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
    preview_autostarted: bool, // auto-on once a phone appears; a manual stop sticks
    preview_q_mode: u8, // 0=Low 1=Med 2=High 3=Auto (UI selection)
    preview_q_auto: u8, // the Auto-adjusted effective level (0..2)
    preview_q_adjust_at: Instant, // cooldown between Auto steps
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
    // Link health (#1/#3/#4): the phone's preview is our only inbound signal, so its
    // arrival = the link is alive. Stall → watchdog re-resolves + redirects.
    last_preview_at: Instant,
    last_reconnect_at: Instant,
    reconnects: u32,
    cam_manual_exp: bool,
    cam_iso: f32,
    cam_shutter: f32,
    cam_ev: f32, // 0.5 = 0 EV
    cam_manual_focus: bool,
    cam_lens: f32,
    cam_ae_lock: bool,
    cam_front: bool, // tracked flip intent (the phone has no ack channel)
    active_cam: u8,  // director slot: 0=0.5× 1=1× 2=tele 3=front (keys 1–4)
    // Pre-broadcast input level meter (so you can SEE signal before hitting ▶).
    level_monitor: Option<LevelMonitor>,
    monitored_device: String,
    // Mic check: a separate input device's level + a headphone monitor (listen), so
    // you can confirm the mic is audible before/while streaming.
    mic_monitor: Option<MicMonitor>,
    mic_device: String,
    mic_monitored: String, // which device the live MicMonitor is on
    mic_listen: bool,
    mic_gain: f32,
    // Remote gallery: browse + pull the phone's recorded clips over WiFi.
    gallery: gallery::Gallery,
    tab: Tab,
    // Multipeer (AWDL) transport — auto-up on launch so the phone is reachable with
    // no WiFi. `mp_started` makes the auto-start happen once (a manual stop sticks).
    multipeer: multipeer::Multipeer,
    mp_last_recv: String,
    mp_started: bool,
    // Transport: WiFi (UDP/Bonjour, the default) or Direct (route audio/preview/control
    // through the Multipeer link — for the field where UDP-over-AWDL won't establish).
    transport_mode: Transport,
    last_keepalive: Instant, // keep the AWDL link warm so it drops less
    mp_tx_audio: Arc<AtomicU64>, // audio packets forwarded over the link (incremented on the forward thread)
    source_listen: bool, // hear the broadcast source on the Mac's own speakers/headphones
    broadcast_autostart: bool, // auto-up the broadcast once on launch; a manual stop sticks
    broadcast_device: String,  // the device the live broadcaster is on (detect a switch)
    local_rec_file: Option<String>, // backup: clean WAV being recorded (synced to the take)
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Preview,
    Gallery,
}

/// Where audio/preview/control are routed. `Auto` prefers WiFi when the phone is on
/// the LAN and falls to the Direct (Multipeer/AWDL) link when WiFi is gone.
#[derive(PartialEq, Clone, Copy)]
enum Transport {
    Auto,
    WiFi,
    Direct,
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
            preview_autostarted: false,
            preview_q_mode: 3, // Auto by default
            preview_q_auto: 1, // start at Med
            preview_q_adjust_at: Instant::now(),
            last_heartbeat: Instant::now(),
            control: RemoteControl::new().ok(),
            sync_test: false,
            last_pulse: Instant::now(),
            flash: false,
            histogram: [0; 64],
            preview_frames: 0,
            preview_fps: 0.0,
            preview_fps_at: Instant::now(),
            last_preview_at: Instant::now(),
            last_reconnect_at: Instant::now(),
            reconnects: 0,
            cam_manual_exp: false,
            cam_iso: 0.3,
            cam_shutter: 0.3,
            cam_ev: 0.5,
            cam_manual_focus: false,
            cam_lens: 0.5,
            cam_ae_lock: false,
            cam_front: false,
            active_cam: 1, // phone starts on the rear wide (1×) lens
            level_monitor: None,
            monitored_device: String::new(),
            mic_monitor: None,
            mic_device: String::new(),
            mic_monitored: String::new(),
            mic_listen: false,
            mic_gain: 1.0,
            gallery: gallery::Gallery::default(),
            tab: Tab::Preview,
            multipeer: multipeer::Multipeer::default(),
            mp_last_recv: String::new(),
            mp_started: false,
            transport_mode: Transport::Auto,
            last_keepalive: Instant::now(),
            mp_tx_audio: Arc::new(AtomicU64::new(0)),
            source_listen: false,
            broadcast_autostart: false,
            broadcast_device: String::new(),
            local_rec_file: None,
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

    /// Keep the mic monitor running on the chosen mic device (independent of the
    /// broadcast source). Restart on device change; drop when no device is picked.
    /// Push the live listen/gain settings to it each frame.
    fn update_mic_monitor(&mut self) {
        if self.mic_device.is_empty() {
            self.mic_monitor = None;
            self.mic_monitored.clear();
            return;
        }
        if self.mic_monitor.is_none() || self.mic_monitored != self.mic_device {
            self.mic_monitor = MicMonitor::start(&self.mic_device).ok();
            self.mic_monitored = self.mic_device.clone();
        }
        if let Some(m) = &self.mic_monitor {
            m.set_listen(self.mic_listen);
            m.set_gain(self.mic_gain);
        }
    }

    /// Send a camera parameter (normalized 0..1) to the phone over the persistent
    /// control socket (same stable port as the other commands).
    fn cam(&self, param: u8, value: f32) {
        if let (Some(addr), Some(c)) = (self.phone(), self.control.as_ref()) {
            let _ = c.send_cam(addr, param, value);
        }
        if self.effective_direct() && self.multipeer.connected() {
            self.multipeer.send(camparam_packet(param, value).to_vec(), true);
        }
    }

    /// Multicam director: punch a camera live (slot 0=0.5× 1=1× 2=tele 3=front).
    /// A rear lens pick auto-flips the phone back to rear in one reconfiguration, so
    /// front→lens is seamless; slot 3 flips to the front camera.
    fn select_cam(&mut self, slot: u8) {
        self.active_cam = slot;
        if slot == 3 {
            self.cam_front = true;
            self.cam(CAM_FLIP, 1.0);
        } else {
            self.cam_front = false; // reelcam forces rear when selecting a lens
            self.cam(CAM_LENS_SELECT, slot as f32);
        }
    }

    /// Effective preview quality level (0..2): the Auto-adjusted value in Auto mode,
    /// else the manual selection.
    fn effective_preview_q(&self) -> u8 {
        if self.preview_q_mode == 3 {
            self.preview_q_auto
        } else {
            self.preview_q_mode.min(2)
        }
    }

    /// Auto quality: nudge the level by the arriving preview fps (a network-health
    /// proxy). Drops on congestion, raises slowly when healthy — hysteresis + cooldown
    /// keep it from oscillating.
    fn adapt_preview_quality(&mut self) {
        if self.preview_q_mode != 3 || self.preview.is_none() {
            return;
        }
        if self.last_preview_at.elapsed() > Duration::from_millis(1500)
            || self.preview_q_adjust_at.elapsed() < Duration::from_secs(3)
        {
            return; // stale frames or still in cooldown
        }
        if self.preview_fps < 7.0 && self.preview_q_auto > 0 {
            self.preview_q_auto -= 1;
            self.preview_q_adjust_at = Instant::now();
        } else if self.preview_fps > 14.0 && self.preview_q_auto < 2 {
            self.preview_q_auto += 1;
            self.preview_q_adjust_at = Instant::now();
        }
    }

    /// Link health from the inbound preview: time since the last frame. `None` when
    /// preview is off (no inbound signal to judge the link by).
    fn link_age(&self) -> Option<Duration> {
        if self.preview.is_some() || self.effective_direct() {
            Some(self.last_preview_at.elapsed())
        } else {
            None
        }
    }

    /// Watchdog (#1): while previewing, if the phone's frames have stalled the link
    /// likely bounced (sleep / range / new AWDL address). Re-resolve the phone from
    /// discovery and redirect a live broadcast — no manual 🔄. Preview/control/heartbeat
    /// all read `phone_addr` live, so they self-heal once it's updated.
    fn link_watchdog(&mut self) {
        // In Direct mode there's no WiFi peer to re-resolve; the bridge re-invites on
        // its own. Don't let a stale WiFi peer churn the broadcaster onto the wrong path.
        if self.effective_direct() {
            return;
        }
        let Some(age) = self.link_age() else { return };
        if age < Duration::from_millis(2500) {
            return; // link healthy
        }
        if self.last_reconnect_at.elapsed() < Duration::from_secs(4) {
            return; // cooldown — give the last attempt time to take
        }
        // Find the phone again: the remembered one, else the only peer present.
        let found = self
            .remembered
            .as_ref()
            .and_then(|n| self.peers.iter().find(|p| &p.name == n))
            .or_else(|| (self.peers.len() == 1).then(|| &self.peers[0]))
            .map(|p| p.addr.to_string());
        let Some(addr) = found else { return };
        self.last_reconnect_at = Instant::now();
        if addr != self.phone_addr {
            self.phone_addr = addr.clone();
            self.reconnects += 1;
            if self.on() {
                self.start(); // drop + rebuild the broadcaster pointed at the new addr
            }
            self.status = format!("link recovered → {addr}");
        } else {
            // Same address — nudge preview to re-arm on the next heartbeat tick.
            self.last_heartbeat =
                Instant::now().checked_sub(Duration::from_secs(3)).unwrap_or_else(Instant::now);
        }
    }

    /// Send a control command to the phone over the persistent socket (stable
    /// source port = one connection on the phone; no flow churn). Returns whether
    /// the datagram went out.
    fn ctl(&self, to: SocketAddr, cmd: u8, port: u16) -> bool {
        if self.effective_direct() && self.multipeer.connected() {
            self.multipeer.send(control_packet(cmd, port).to_vec(), true);
        }
        self.control.as_ref().is_some_and(|c| c.send(to, cmd, port).is_ok())
    }

    fn on(&self) -> bool {
        self.broadcaster.is_some()
    }

    /// The phone's control endpoint (ip:port parsed from the target field).
    fn phone(&self) -> Option<SocketAddr> {
        self.phone_addr.parse::<SocketAddr>().ok()
    }

    /// Should we route over the Direct (Multipeer/AWDL) link right now? In Auto, yes
    /// only when WiFi is gone (no phone discovered on the LAN) but the link is up — so
    /// it prefers WiFi and falls back to Direct automatically.
    fn effective_direct(&self) -> bool {
        match self.transport_mode {
            Transport::WiFi => false,
            Transport::Direct => true,
            Transport::Auto => self.peers.is_empty() && self.multipeer.connected(),
        }
    }

    /// Is the phone reachable for control — a WiFi IP, OR the Direct (Multipeer) link?
    fn phone_reachable(&self) -> bool {
        self.phone().is_some() || (self.effective_direct() && self.multipeer.connected())
    }

    /// Toggle recording ON THE PHONE (no ack — we track intent locally). Works over the
    /// Multipeer link in Direct mode (no WiFi IP needed). Also records a synced LOCAL
    /// clean-audio WAV backup (+ a start click as the post-merge sync anchor).
    fn toggle_remote_record(&mut self) {
        let sent = if self.effective_direct() && self.multipeer.connected() {
            self.multipeer.send(control_packet(CMD_REC_TOGGLE, 0).to_vec(), true);
            true
        } else if let Some(addr) = self.phone() {
            self.ctl(addr, CMD_REC_TOGGLE, 0)
        } else {
            false
        };
        if !sent {
            return;
        }
        self.remote_rec = !self.remote_rec;
        // Local clean-audio backup, in sync with the phone's take.
        if self.remote_rec {
            let path = backup_audio_path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            if let Some(b) = &self.broadcaster {
                b.pulse(); // sync CLICK at the start — landmark for post-merge alignment
                b.record_start(path);
                self.local_rec_file = Some(name);
            }
        } else if let Some(b) = &self.broadcaster {
            b.record_stop();
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
        // Start a UDP receiver (used in WiFi mode; harmless in Direct, where frames
        // arrive over Multipeer). Tell the phone where to send — over whichever
        // transport is active (the heartbeat keeps re-asserting it).
        if let Ok(rx) = PreviewReceiver::start() {
            let port = rx.port();
            if self.effective_direct() && self.multipeer.connected() {
                self.multipeer.send(control_packet(CMD_PREVIEW_START, port).to_vec(), true);
            } else if let Some(addr) = self.phone() {
                self.ctl(addr, CMD_PREVIEW_START, port);
            }
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
        let Some(jpeg) = rx.latest_jpeg() else { return };
        self.show_jpeg(ctx, &jpeg);
    }

    /// Decode a preview JPEG → texture + histogram and tick the fps/link meters.
    /// Shared by the UDP receiver and the Multipeer (Direct) preview path.
    fn show_jpeg(&mut self, ctx: &egui::Context, jpeg: &[u8]) {
        self.last_preview_at = Instant::now(); // link is alive (inbound frame)
        // FPS meter: count displayed frames, recompute once a second.
        self.preview_frames += 1;
        let dt = self.preview_fps_at.elapsed().as_secs_f32();
        if dt >= 1.0 {
            self.preview_fps = self.preview_frames as f32 / dt;
            self.preview_frames = 0;
            self.preview_fps_at = Instant::now();
        }
        if let Ok(img) = image::load_from_memory(jpeg) {
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
        // #3 Link-health bar — the inbound preview rate doubles as link quality.
        ui.horizontal(|ui| {
            match self.link_age() {
                Some(age) if age < Duration::from_millis(800) => {
                    ui.colored_label(
                        egui::Color32::from_rgb(90, 220, 130),
                        format!("● link · {:.0} fps", self.preview_fps),
                    );
                }
                Some(age) if age < Duration::from_millis(2500) => {
                    ui.colored_label(egui::Color32::from_rgb(220, 200, 90), "◐ link weak");
                }
                Some(_) => {
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 70, 70),
                        "○ link lost — recovering…",
                    );
                }
                None => {
                    ui.weak(format!("📊 {:.0} fps (preview off)", self.preview_fps));
                }
            }
            self.draw_histogram(ui);
        });
        if self.reconnects > 0 {
            ui.weak(format!("auto-reconnects this session: {}", self.reconnects));
        }
        // Multicam director — punch the phone's camera live. Keys 1–4 mirror these.
        ui.horizontal(|ui| {
            ui.label("🎬");
            for (slot, label) in
                [(0u8, "1·0.5×"), (1, "2·1×"), (2, "3·Tele"), (3, "4·Front")]
            {
                let btn = egui::Button::new(label)
                    .min_size(egui::vec2(64.0, 30.0))
                    .fill(if self.active_cam == slot {
                        egui::Color32::from_rgb(180, 40, 40) // tally red = on air
                    } else {
                        egui::Color32::from_gray(52)
                    });
                if ui.add(btn).clicked() {
                    self.select_cam(slot);
                }
            }
        });
        ui.weak("keys 1–4 switch the camera live");
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
        // Direct transport has no UDP target (no WiFi/discovery in the field) — the
        // audio rides Multipeer. Use a dummy addr so the broadcaster still runs + taps.
        let addr = self
            .phone_addr
            .parse::<SocketAddr>()
            .ok()
            .or_else(|| self.effective_direct().then(|| "127.0.0.1:9000".parse().unwrap()));
        match addr {
            Some(addr) => match Broadcaster::start(&self.sel_device, addr, self.sample_rate) {
                Ok(mut b) => {
                    // Stream the broadcaster's audio packets over the Multipeer link on a
                    // DEDICATED thread (smooth, like the UDP path) — not the UI loop. The
                    // thread ends when the broadcaster drops (tap sender closes). Only take
                    // the tap if the bridge is up (else the receiver would be lost for good).
                    if let Some(tx) = self.multipeer.sender() {
                        if let Some(rx) = b.take_tap() {
                            let counter = self.mp_tx_audio.clone();
                            std::thread::Builder::new()
                                .name("mp-audio-forward".into())
                                .spawn(move || {
                                    while let Ok(pkt) = rx.recv() {
                                        let mut frame = Vec::with_capacity(1 + pkt.len());
                                        frame.push(0u8); // unreliable, like UDP
                                        frame.extend_from_slice(&pkt);
                                        if tx.send(frame).is_err() {
                                            break; // bridge gone
                                        }
                                        counter.fetch_add(1, Ordering::Relaxed);
                                    }
                                })
                                .ok();
                        }
                    }
                    self.broadcaster = Some(b);
                    self.broadcast_device = self.sel_device.clone();
                    self.status = format!("streaming → {addr}");
                    if let Some(name) =
                        self.peers.iter().find(|p| p.addr == addr).map(|p| p.name.clone())
                    {
                        self.remembered = Some(name.clone());
                        save_last_phone(&name, &addr.to_string());
                    } else if !self.effective_direct() {
                        save_last_phone("", &addr.to_string());
                    }
                }
                Err(e) => self.status = format!("error: {e}"),
            },
            None => self.status = "bad ip:port (e.g. 172.20.10.2:9000)".into(),
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
        // Keep the mic check (level + headphone monitor) alive on its device.
        self.update_mic_monitor();

        // Auto-start the broadcast once on launch (so you don't forget to hit ▶), as
        // soon as a source device + a target (phone IP or the Direct link) are ready.
        // A manual stop then sticks — we don't re-arm.
        if !self.broadcast_autostart
            && !self.on()
            && !self.sel_device.is_empty()
            && (self.phone().is_some() || self.effective_direct())
        {
            self.start();
            if self.on() {
                self.broadcast_autostart = true;
            }
        }
        // Source device switched mid-broadcast → rebuild the broadcaster on the new one
        // (otherwise it keeps capturing the old device — the "nothing happens" gotcha).
        if self.on() && self.broadcast_device != self.sel_device {
            self.start();
        }

        // Multicam director hotkeys: 1/2/3/4 → 0.5× / 1× / tele / front, live on the
        // phone. Skipped while a text field (e.g. the phone-IP box) has focus.
        if ctx.memory(|m| m.focused()).is_none() {
            for (slot, key) in
                [egui::Key::Num1, egui::Key::Num2, egui::Key::Num3, egui::Key::Num4]
                    .into_iter()
                    .enumerate()
            {
                if ctx.input(|i| i.key_pressed(key)) {
                    self.select_cam(slot as u8);
                }
            }
        }

        // Auto-start the phone preview once a phone is reachable — the director monitor
        // (and the link watchdog/health/alert, which key off inbound frames) come up
        // with no click. Retries each frame until a phone appears; then a manual stop
        // sticks. Skipped on a transient gap so it doesn't fight a manual stop.
        let phone_reachable =
            self.phone().is_some() || (self.effective_direct() && self.multipeer.connected());
        if !self.preview_autostarted && self.preview.is_none() && phone_reachable {
            self.toggle_preview();
            if self.preview.is_some() {
                self.preview_autostarted = true;
            }
        }

        // Link watchdog: auto-recover a stalled phone link (no manual 🔄 refresh).
        self.link_watchdog();

        // Auto-up the AWDL link once on launch (the phone is then reachable with no
        // WiFi). A manual stop sticks — we don't re-spawn.
        if !self.mp_started {
            self.multipeer.start();
            self.mp_started = true;
        }

        // Pull the latest camera-preview frame (if any) into the texture.
        self.poll_preview(ctx);
        // Drain remote-gallery background results (list / thumbs / downloads).
        self.gallery.poll(ctx);
        // Drain Multipeer frames (Phase 1: show the last as text for the round-trip test).
        // Inbound over the Multipeer link: preview frames (Direct transport) → display;
        // anything else → debug line.
        for f in self.multipeer.drain() {
            if f.len() > 8 && u32::from_le_bytes([f[0], f[1], f[2], f[3]]) == PREVIEW_MAGIC {
                self.show_jpeg(ctx, &f[8..]);
            } else {
                self.mp_last_recv = String::from_utf8_lossy(&f).chars().take(60).collect();
            }
        }
        // Keep the AWDL link warm so it drops less — a tiny periodic ping keeps the
        // MCSession active even when idle (e.g. Auto is currently on WiFi, or Direct is
        // up but not broadcasting). The phone ignores it (no audio/control magic).
        if self.multipeer.connected() && self.last_keepalive.elapsed() >= Duration::from_millis(1000) {
            self.last_keepalive = Instant::now();
            self.multipeer.send(vec![0u8; 4], true);
        }

        // Direct transport: the broadcaster only FILLS its tap in Direct mode; a
        // dedicated thread (spawned in start()) streams those packets over the link
        // smoothly. (No UI-loop draining — that batched audio into ~10 Hz bursts.)
        if let Some(b) = &self.broadcaster {
            // Fill the tap only with a live link to forward to (else packets pile up
            // for a bridge that drops them anyway).
            b.set_tap(self.effective_direct() && self.multipeer.connected());
            b.set_monitor(self.source_listen); // local speaker monitor of the source
        }

        // Heartbeat: while previewing, re-arm the phone's watchdog (~every 2 s) so
        // it keeps streaming; if we crash, it self-stops after the timeout.
        if self.preview.is_some() && self.last_heartbeat.elapsed() >= Duration::from_secs(2) {
            self.last_heartbeat = Instant::now();
            let port = self.preview.as_ref().map(|r| r.port()).unwrap_or(0);
            let q = self.effective_preview_q() as u16;
            if self.effective_direct() && self.multipeer.connected() {
                // Field path: no UDP addr — drive preview over the Multipeer link.
                self.multipeer.send(control_packet(CMD_PREVIEW_START, port).to_vec(), true);
                self.multipeer.send(control_packet(CMD_PREVIEW_QUALITY, q).to_vec(), true);
            } else if let Some(addr) = self.phone() {
                self.ctl(addr, CMD_PREVIEW_START, port);
                // Re-assert quality (survives a phone-side preview restart that resets it).
                self.ctl(addr, CMD_PREVIEW_QUALITY, q);
            }
        }

        // Auto quality: in Auto mode, nudge the level by the arriving preview fps —
        // network health proxy. Drop fast on congestion, raise slowly when healthy.
        self.adapt_preview_quality();

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

                    // Unified phone-link status — one place, both transports. WiFi =
                    // discovered over mDNS; AWDL = the Multipeer link (no router/SIM).
                    let wifi = !self.peers.is_empty();
                    let awdl = self.multipeer.connected();
                    let (status, col) = match (wifi, awdl) {
                        (true, true) => ("📱 Phone: connected (WiFi + AWDL link)", egui::Color32::from_rgb(90, 220, 130)),
                        (false, true) => ("📱 Phone: connected (AWDL link · no WiFi)", egui::Color32::from_rgb(90, 220, 130)),
                        (true, false) => ("📱 Phone: on WiFi", egui::Color32::from_rgb(220, 200, 90)),
                        (false, false) => ("📱 Phone: searching… (open Reelcam)", egui::Color32::from_gray(150)),
                    };
                    ui.colored_label(col, status);

                    // Discovered phones (for targeting when several are on WiFi).
                    if self.peers.is_empty() {
                        if awdl {
                            ui.weak("linked directly — no WiFi address to pick");
                        }
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
                    // Listen to the source on THIS Mac — for when it's going only to the
                    // phone (e.g. REAPER → BlackHole) and you can't hear it locally.
                    let listen_lbl = if self.source_listen {
                        "🔊 listening on this Mac"
                    } else {
                        "🔇 listen on this Mac"
                    };
                    if ui.selectable_label(self.source_listen, listen_lbl).clicked() {
                        self.source_listen = !self.source_listen;
                    }
                    if self.source_listen && !self.on() {
                        ui.weak("starts when you ▶ broadcast");
                    }

                    // ── 🎤 Mic check — level + headphone monitor (is it audible?) ──
                    ui.add_space(8.0);
                    ui.separator();
                    ui.label("🎤 Mic check:");
                    egui::ComboBox::from_id_salt("micdev")
                        .selected_text(if self.mic_device.is_empty() {
                            "— off —"
                        } else {
                            self.mic_device.as_str()
                        })
                        .width(300.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.mic_device, String::new(), "— off —");
                            for d in &self.devices {
                                ui.selectable_value(&mut self.mic_device, d.clone(), d.as_str());
                            }
                        });
                    if !self.mic_device.is_empty() {
                        let mlevel = self.mic_monitor.as_ref().map(|m| m.level()).unwrap_or(0.0);
                        ui.add_space(2.0);
                        level_bar(ui, mlevel, true);
                        ui.horizontal(|ui| {
                            let lbl = if self.mic_listen { "🎧 monitor ON" } else { "🎧 monitor off" };
                            if ui.selectable_label(self.mic_listen, lbl).clicked() {
                                self.mic_listen = !self.mic_listen;
                            }
                            ui.add(egui::Slider::new(&mut self.mic_gain, 0.0..=2.0).text("vol"));
                        });
                        if self.mic_listen {
                            ui.colored_label(
                                egui::Color32::from_rgb(230, 170, 70),
                                "⚠ use headphones — speakers + open mic = feedback",
                            );
                        } else if mlevel < 0.001 {
                            ui.weak("no mic signal — check the input / preamp gain");
                        }
                    }

                    // BIG phone-remote buttons.
                    ui.add_space(8.0);
                    ui.separator();
                    ui.label("📲 Phone remote:");
                    let has_phone = self.phone().is_some(); // WiFi IP — for the HTTP gallery
                    let can_control = self.phone_reachable(); // IP OR the Direct link
                    let rec = if self.remote_rec { "■ stop rec" } else { "⏺ record" };
                    let rec_btn = egui::Button::new(egui::RichText::new(rec).size(16.0))
                        .min_size(egui::vec2(300.0, 42.0))
                        .fill(if self.remote_rec {
                            egui::Color32::from_rgb(120, 40, 40)
                        } else {
                            egui::Color32::from_gray(52)
                        });
                    if ui.add_enabled(can_control, rec_btn).clicked() {
                        self.toggle_remote_record();
                    }
                    // Local clean-audio backup status (the post-merge safety net).
                    if self.remote_rec {
                        match &self.local_rec_file {
                            Some(f) => ui.colored_label(
                                egui::Color32::from_rgb(210, 90, 90),
                                format!("● REC + 💾 clean audio → {f}"),
                            ),
                            None => ui.colored_label(
                                egui::Color32::from_rgb(230, 170, 70),
                                "● REC (no local backup — start ▶ broadcast)",
                            ),
                        };
                    } else if let Some(f) = &self.local_rec_file {
                        ui.weak(format!("💾 saved clean audio: {f}"));
                    }
                    ui.add_space(4.0);
                    let prev = if self.preview.is_some() { "👁 preview: ON" } else { "👁 preview: off" };
                    if ui
                        .add_enabled(
                            can_control,
                            egui::Button::new(prev).min_size(egui::vec2(300.0, 34.0)),
                        )
                        .clicked()
                    {
                        self.toggle_preview();
                    }
                    // Preview quality — Low/Med/High, or Auto (drops under network load).
                    if self.preview.is_some() {
                        ui.horizontal(|ui| {
                            ui.label("Quality:");
                            for (i, name) in ["Low", "Med", "High", "Auto"].iter().enumerate() {
                                if ui
                                    .selectable_label(self.preview_q_mode == i as u8, *name)
                                    .clicked()
                                {
                                    self.preview_q_mode = i as u8;
                                    let q = self.effective_preview_q();
                                    if self.effective_direct() && self.multipeer.connected() {
                                        self.multipeer
                                            .send(control_packet(CMD_PREVIEW_QUALITY, q as u16).to_vec(), true);
                                    } else if let Some(addr) = self.phone() {
                                        self.ctl(addr, CMD_PREVIEW_QUALITY, q as u16);
                                    }
                                }
                            }
                        });
                        if self.preview_q_mode == 3 {
                            let lvl = ["Low", "Med", "High"][self.preview_q_auto.min(2) as usize];
                            ui.weak(format!("auto → {lvl} · {:.0} fps", self.preview_fps));
                        }
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

                    // ── Direct AWDL link (auto) — no router / SIM / internet ──
                    // Auto-up on launch; collapsed to a small control since it's hands-off.
                    ui.add_space(8.0);
                    ui.separator();
                    let mp_on = self.multipeer.running();
                    ui.horizontal(|ui| {
                        let dot = if self.multipeer.connected() {
                            "🟢"
                        } else if mp_on {
                            "🟡"
                        } else {
                            "⚪"
                        };
                        ui.weak(format!("{dot} direct link (AWDL)"));
                        let toggle = if mp_on { "stop" } else { "start" };
                        if ui.small_button(toggle).clicked() {
                            if mp_on {
                                self.multipeer.stop();
                            } else {
                                self.multipeer.start();
                            }
                            self.mp_started = true; // a manual choice sticks
                        }
                        if mp_on
                            && ui
                                .add_enabled(self.multipeer.connected(), egui::Button::new("ping").small())
                                .clicked()
                        {
                            self.multipeer.send(b"ping".to_vec(), true);
                        }
                    });
                    if mp_on && !self.mp_last_recv.is_empty() {
                        ui.weak(format!("recv: {}", self.mp_last_recv));
                    }
                    // Transport selector. Auto = WiFi when the phone is on the LAN, else
                    // the Direct (Multipeer/AWDL) link — the no-WiFi field path.
                    ui.horizontal(|ui| {
                        ui.label("Transport:");
                        ui.selectable_value(&mut self.transport_mode, Transport::Auto, "Auto");
                        ui.selectable_value(&mut self.transport_mode, Transport::WiFi, "WiFi");
                        ui.selectable_value(&mut self.transport_mode, Transport::Direct, "Direct");
                    });
                    // Show which transport is EFFECTIVE right now (esp. in Auto).
                    if self.effective_direct() {
                        let conn = self.multipeer.connected();
                        let col = if conn {
                            egui::Color32::from_rgb(90, 220, 130)
                        } else {
                            egui::Color32::from_rgb(230, 70, 70)
                        };
                        ui.colored_label(
                            col,
                            if conn { "→ Direct link: connected" } else { "→ Direct: NOT connected" },
                        );
                        ui.weak(format!(
                            "tx {} audio pkts · preview {:.0} fps{}",
                            self.mp_tx_audio.load(Ordering::Relaxed),
                            self.preview_fps,
                            if self.on() { "" } else { " · (hit ▶ broadcast for audio)" },
                        ));
                        let st = self.multipeer.status();
                        if !st.is_empty() {
                            ui.weak(format!("bridge: {st}"));
                        }
                    } else if self.transport_mode == Transport::Auto {
                        if !self.peers.is_empty() {
                            ui.colored_label(
                                egui::Color32::from_rgb(90, 220, 130),
                                "→ WiFi (phone on the LAN)",
                            );
                        } else {
                            ui.colored_label(
                                egui::Color32::from_rgb(220, 200, 90),
                                "→ searching… (no WiFi; Direct link not up yet)",
                            );
                        }
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

        // #4 Alert: broadcasting but the phone link is lost → the reel is losing its
        // clean audio. A bright red banner over everything so you catch it mid-set.
        if self.on() && self.link_age().is_some_and(|a| a > Duration::from_millis(2500)) {
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("link_alert"),
            ));
            let full = ctx.content_rect();
            let bar = egui::Rect::from_min_size(full.min, egui::vec2(full.width(), 26.0));
            painter.rect_filled(bar, 0.0, egui::Color32::from_rgb(200, 30, 30));
            painter.text(
                bar.center(),
                egui::Align2::CENTER_CENTER,
                "⚠ LINK LOST — phone not receiving audio",
                egui::FontId::proportional(14.0),
                egui::Color32::WHITE,
            );
        }

        // Repaint cadence: crisp 60 fps during the sync test (well-timed flash),
        // ~6 fps while previewing, else a lazy 2 fps heartbeat.
        let ms = if self.sync_test {
            16
        } else if self.preview.is_some() {
            66 // ~15 fps preview refresh → show the freshest frame sooner
        } else if self.on()
            || self.level_monitor.is_some()
            || self.mic_monitor.is_some()
            || (self.gallery.open && self.gallery.busy)
        {
            100 // ~10 fps: audio level meter (incl. pre-broadcast + mic) / gallery spinner
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

/// Path for a backup clean-audio file: ~/Movies/Reelcam/beam-audio-<unix_secs>.m4a
/// (AAC — directly mergeable onto the phone video; falls back to .wav if no ffmpeg).
fn backup_audio_path() -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Movies/Reelcam");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("beam-audio-{ts}.m4a"))
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

