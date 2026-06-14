//! **beam** — a tiny standalone window that captures an input device (BlackHole
//! carrying REAPER, system audio, a hardware input) and UDP-streams it to the
//! phone (reelcam) over WiFi. The "just send audio, no DJ console" tool split out
//! of livehub. Reuses `livehub-engine`'s `Broadcaster` + `Discovery`.
//!
//! A small always-on-top window (not a menu-bar item — a full menu bar can hide
//! those): pick Source + Phone (auto-discovered), hit ▶. Closing the window quits
//! (and stops the stream); ⌘H tucks it away while it keeps broadcasting.

use std::net::SocketAddr;
use std::time::Duration;

use livehub_engine::{
    input_device_names, send_control, Broadcaster, Discovery, Peer, PreviewReceiver,
    CMD_PREVIEW_START, CMD_PREVIEW_STOP, CMD_REC_TOGGLE,
};

fn main() -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([360.0, 300.0])
            .with_min_inner_size([320.0, 240.0])
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
            discovery: Discovery::start().ok(),
            peers: Vec::new(),
            phone_addr: saved_addr.unwrap_or_default(),
            remembered,
            broadcaster: None,
            status: String::new(),
            remote_rec: false,
            preview: None,
            preview_tex: None,
            preview_seq: 0,
        }
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
            if send_control(addr, CMD_REC_TOGGLE, 0).is_ok() {
                self.remote_rec = !self.remote_rec;
            }
        }
    }

    /// Start/stop receiving the phone's camera preview (MJPEG → one texture).
    fn toggle_preview(&mut self) {
        if self.preview.is_some() {
            if let Some(addr) = self.phone() {
                let _ = send_control(addr, CMD_PREVIEW_STOP, 0);
            }
            self.preview = None;
            self.preview_tex = None; // drop the texture → free GPU memory
            return;
        }
        // Start a receiver on an ephemeral port, tell the phone to send there.
        if let (Some(addr), Ok(rx)) = (self.phone(), PreviewReceiver::start()) {
            let _ = send_control(addr, CMD_PREVIEW_START, rx.port());
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
        if let Ok(img) = image::load_from_memory(&jpeg) {
            let rgba = img.to_rgba8();
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

    fn start(&mut self) {
        match self.phone_addr.parse::<SocketAddr>() {
            Ok(addr) => match Broadcaster::start(&self.sel_device, addr) {
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

        // Pull the latest camera-preview frame (if any) into the texture.
        self.poll_preview(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(4.0);
            ui.heading("📡 beam");
            ui.label("Stream an input device to your phone (reelcam).");
            ui.separator();

            // ── Discovered phones ──────────────────────────────────────────
            if self.peers.is_empty() {
                ui.weak("📱 searching for reelcam on the network…");
            } else {
                ui.label("📱 found phones (click to target):");
                for p in &self.peers {
                    let star = if self.remembered.as_deref() == Some(p.name.as_str()) {
                        "⭐ "
                    } else {
                        ""
                    };
                    if ui
                        .button(format!("{star}{} · {}", p.name, p.addr))
                        .clicked()
                    {
                        self.phone_addr = p.addr.to_string();
                        self.remembered = Some(p.name.clone());
                        save_last_phone(&p.name, &self.phone_addr);
                    }
                }
            }
            ui.add_space(6.0);

            // ── Source device ──────────────────────────────────────────────
            ui.label("Source device:");
            egui::ComboBox::from_id_salt("dev")
                .selected_text(if self.sel_device.is_empty() {
                    "— pick —"
                } else {
                    self.sel_device.as_str()
                })
                .width(320.0)
                .show_ui(ui, |ui| {
                    for d in &self.devices {
                        ui.selectable_value(&mut self.sel_device, d.clone(), d.as_str());
                    }
                });

            ui.add_space(4.0);
            ui.label("Phone (ip:port):");
            ui.add_enabled(
                !self.on(),
                egui::TextEdit::singleline(&mut self.phone_addr)
                    .hint_text("172.20.10.2:9000")
                    .desired_width(320.0),
            );

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let label = if self.on() { "■ stop" } else { "▶ broadcast" };
                if ui.add_sized([110.0, 28.0], egui::Button::new(label)).clicked() {
                    if self.on() {
                        self.broadcaster = None;
                        self.status = "stopped".into();
                    } else {
                        self.start();
                    }
                }
                if self.on() {
                    ui.colored_label(egui::Color32::from_rgb(90, 210, 120), "● live");
                }
            });
            if !self.status.is_empty() {
                ui.add_space(2.0);
                ui.weak(&self.status);
            }

            // ── Level meter (of the captured source, while broadcasting) ──────
            ui.add_space(6.0);
            let level = self.broadcaster.as_ref().map(|b| b.level()).unwrap_or(0.0);
            level_bar(ui, level, self.on());
            if self.on() && level < 0.001 {
                ui.colored_label(
                    egui::Color32::from_rgb(230, 170, 70),
                    "⚠ source is silent — is signal reaching this device?",
                );
            }

            // ── Phone remote + camera preview ─────────────────────────────────
            ui.add_space(6.0);
            ui.separator();
            ui.label("📲 Phone remote:");
            ui.horizontal(|ui| {
                let has_phone = self.phone().is_some();
                let rec = if self.remote_rec { "■ stop rec" } else { "⏺ record" };
                if ui.add_enabled(has_phone, egui::Button::new(rec)).clicked() {
                    self.toggle_remote_record();
                }
                let prev = if self.preview.is_some() { "👁 preview ✓" } else { "👁 preview" };
                if ui.add_enabled(has_phone, egui::Button::new(prev)).clicked() {
                    self.toggle_preview();
                }
                if self.remote_rec {
                    ui.colored_label(egui::Color32::from_rgb(235, 90, 90), "● REC");
                }
            });
            if self.preview.is_some() {
                if let Some(tex) = &self.preview_tex {
                    let avail = ui.available_width().min(240.0);
                    let sz = tex.size_vec2();
                    let aspect = if sz.x > 0.0 { sz.y / sz.x } else { 16.0 / 9.0 };
                    let disp = egui::vec2(avail, avail * aspect);
                    ui.add(egui::Image::new(egui::load::SizedTexture::new(tex.id(), disp)));
                } else {
                    ui.weak("waiting for preview frames…");
                }
            }

            // ── System-audio hint ─────────────────────────────────────────────
            ui.add_space(6.0);
            ui.separator();
            ui.weak(
                "🔊 System/YouTube audio isn't captured directly (macOS has no system-\
                 capture device). Route it through BlackHole: Audio MIDI Setup → a Multi-\
                 Output (BlackHole + speakers) → set as output, then pick BlackHole above.",
            );
        });

        // Idle-friendly heartbeat. Faster while showing the live preview so frames
        // flow (~6 fps); otherwise 2 fps just to refresh the phone list + meter.
        let ms = if self.preview.is_some() { 160 } else { 500 };
        ctx.request_repaint_after(Duration::from_millis(ms));
    }
}

impl Drop for Beam {
    fn drop(&mut self) {
        // Tell the phone to stop its preview so it doesn't keep sending to a dead
        // port after we quit. (The phone also self-stops on a control timeout.)
        if self.preview.is_some() {
            if let Some(addr) = self.phone() {
                let _ = send_control(addr, CMD_PREVIEW_STOP, 0);
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

// ── Remembered phone ─────────────────────────────────────────────────────────

fn last_phone_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = std::path::Path::new(&home).join("Library/Application Support/beam");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("last_phone.txt"))
}

fn load_last_phone() -> (Option<String>, Option<String>) {
    let Some(p) = last_phone_path() else { return (None, None) };
    let Ok(s) = std::fs::read_to_string(&p) else { return (None, None) };
    let (name, addr) = s.trim().split_once('\t').unwrap_or(("", s.trim()));
    let name = if name.is_empty() { None } else { Some(name.to_string()) };
    let addr = if addr.is_empty() { None } else { Some(addr.to_string()) };
    (name, addr)
}

fn save_last_phone(name: &str, addr: &str) {
    if let Some(p) = last_phone_path() {
        let _ = std::fs::write(p, format!("{name}\t{addr}"));
    }
}
