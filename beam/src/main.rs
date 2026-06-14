//! **beam** — a tiny macOS menu-bar broadcaster. Capture an input device
//! (BlackHole carrying REAPER, system audio, a hardware input) and UDP-stream it
//! to the phone (reelcam) over WiFi — the "just send audio, no DJ console" tool
//! split out of livehub. Reuses `livehub-engine`'s `Broadcaster` + `Discovery`.
//!
//! No window, no dock icon (Accessory activation): it lives in the status bar.
//! Click → pick Source + Phone (auto-discovered) → toggle ▶ Broadcast. The event
//! loop blocks on `WaitUntil` (≈0 % CPU), waking ~1×/s to refresh the phone list.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use livehub_engine::{input_device_names, Broadcaster, Discovery, Peer};
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
use winit::window::WindowId;

const PORT_HINT: u16 = 9000; // reelcam listens here; shown if a peer has no port.

fn main() -> anyhow::Result<()> {
    let event_loop = EventLoop::builder()
        .with_activation_policy(ActivationPolicy::Accessory) // no dock icon
        .build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = Beam::new();
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// All app state. The tray + menu are (re)built on the main thread.
struct Beam {
    devices: Vec<String>,
    sel_device: Option<String>,
    discovery: Option<Discovery>,
    peers: Vec<Peer>,
    sel_phone: Option<SocketAddr>,
    remembered: Option<String>, // phone Bonjour name, persisted across runs
    broadcaster: Option<Broadcaster>,
    tray: Option<TrayIcon>,
    last_poll: Instant,

    // MenuId → action maps, rebuilt with the menu each time.
    dev_ids: Vec<(MenuId, String)>,
    phone_ids: Vec<(MenuId, SocketAddr)>,
    toggle_id: Option<MenuId>,
    quit_id: Option<MenuId>,
}

impl Beam {
    fn new() -> Self {
        let (remembered, saved_addr) = load_last_phone();
        Beam {
            devices: input_device_names(),
            sel_device: None,
            discovery: Discovery::start().ok(),
            peers: Vec::new(),
            sel_phone: saved_addr,
            remembered,
            broadcaster: None,
            tray: None,
            last_poll: Instant::now(),
            dev_ids: Vec::new(),
            phone_ids: Vec::new(),
            toggle_id: None,
            quit_id: None,
        }
    }

    fn is_on(&self) -> bool {
        self.broadcaster.is_some()
    }

    /// Build the whole status-bar menu from current state and install it.
    fn rebuild_menu(&mut self) {
        let menu = Menu::new();
        self.dev_ids.clear();
        self.phone_ids.clear();

        // ── Source submenu ────────────────────────────────────────────────
        let src_label = format!(
            "Source: {}",
            self.sel_device.as_deref().unwrap_or("— pick —")
        );
        let src = Submenu::new(src_label, true);
        if self.devices.is_empty() {
            let _ = src.append(&MenuItem::new("no input devices", false, None));
        } else {
            for d in &self.devices {
                let checked = self.sel_device.as_deref() == Some(d.as_str());
                let item = CheckMenuItem::new(d, true, checked, None);
                self.dev_ids.push((item.id().clone(), d.clone()));
                let _ = src.append(&item);
            }
        }

        // ── Phone submenu ─────────────────────────────────────────────────
        let phone_label = format!(
            "Phone: {}",
            self.sel_phone
                .map(|a| a.to_string())
                .unwrap_or_else(|| "— searching —".into())
        );
        let phone = Submenu::new(phone_label, true);
        if self.peers.is_empty() {
            let _ = phone.append(&MenuItem::new("searching for reelcam…", false, None));
        } else {
            for p in &self.peers {
                let star = if self.remembered.as_deref() == Some(p.name.as_str()) {
                    "⭐ "
                } else {
                    ""
                };
                let checked = self.sel_phone == Some(p.addr);
                let label = format!("{star}{} · {}", p.name, p.addr);
                let item = CheckMenuItem::new(label, true, checked, None);
                self.phone_ids.push((item.id().clone(), p.addr));
                let _ = phone.append(&item);
            }
        }

        // ── Broadcast toggle + status ─────────────────────────────────────
        let ready = self.sel_device.is_some() && self.sel_phone.is_some();
        let toggle = CheckMenuItem::new("▶ Broadcast", ready || self.is_on(), self.is_on(), None);
        self.toggle_id = Some(toggle.id().clone());

        let status = if self.is_on() {
            format!(
                "● streaming → {}",
                self.sel_phone.map(|a| a.to_string()).unwrap_or_default()
            )
        } else if ready {
            "ready — click ▶ Broadcast".into()
        } else {
            "pick a Source and a Phone".into()
        };
        let status_item = MenuItem::new(status, false, None);

        let quit = MenuItem::new("Quit beam", true, None);
        self.quit_id = Some(quit.id().clone());

        let _ = menu.append_items(&[
            &src,
            &phone,
            &PredefinedMenuItem::separator(),
            &toggle,
            &status_item,
            &PredefinedMenuItem::separator(),
            &quit,
        ]);

        if let Some(tray) = &self.tray {
            tray.set_menu(Some(Box::new(menu)));
        }
    }

    /// Refresh the discovered-phone list; auto-pick the remembered phone (even on
    /// a new IP) or the only one present. Returns true if anything changed.
    fn poll_discovery(&mut self) -> bool {
        let Some(d) = &self.discovery else { return false };
        let peers = d.peers();
        let mut changed = peers.len() != self.peers.len()
            || peers.iter().zip(&self.peers).any(|(a, b)| a.addr != b.addr);
        self.peers = peers;

        if !self.is_on() {
            // Remembered phone reappeared → re-target it (IP may have changed).
            if let Some(name) = &self.remembered {
                if let Some(p) = self.peers.iter().find(|p| &p.name == name) {
                    if self.sel_phone != Some(p.addr) {
                        self.sel_phone = Some(p.addr);
                        changed = true;
                    }
                }
            }
            // Nothing chosen yet + exactly one phone → just pick it.
            if self.sel_phone.is_none() && self.peers.len() == 1 {
                self.sel_phone = Some(self.peers[0].addr);
                changed = true;
            }
        }
        changed
    }

    /// Start or stop the UDP broadcast with the current Source + Phone.
    fn toggle_broadcast(&mut self) {
        if self.is_on() {
            self.broadcaster = None; // drop → stop
            return;
        }
        let (Some(dev), Some(addr)) = (self.sel_device.clone(), self.sel_phone) else {
            return;
        };
        match Broadcaster::start(&dev, addr) {
            Ok(b) => {
                self.broadcaster = Some(b);
                // Remember the phone by name (if a peer matches the addr).
                if let Some(name) = self.peers.iter().find(|p| p.addr == addr).map(|p| p.name.clone())
                {
                    self.remembered = Some(name.clone());
                    save_last_phone(&name, &addr.to_string());
                }
            }
            Err(e) => eprintln!("beam: broadcast failed: {e}"),
        }
    }

    /// Handle one menu click by id.
    fn on_menu(&mut self, id: &MenuId) {
        if Some(id) == self.quit_id.as_ref() {
            std::process::exit(0);
        }
        if Some(id) == self.toggle_id.as_ref() {
            self.toggle_broadcast();
            self.rebuild_menu();
            return;
        }
        if let Some((_, dev)) = self.dev_ids.iter().find(|(mid, _)| mid == id) {
            self.sel_device = Some(dev.clone());
            if self.is_on() {
                self.broadcaster = None; // source changed → stop; user re-toggles
            }
            self.rebuild_menu();
            return;
        }
        if let Some((_, addr)) = self.phone_ids.iter().find(|(mid, _)| mid == id) {
            let addr = *addr;
            self.sel_phone = Some(addr);
            if let Some(name) = self.peers.iter().find(|p| p.addr == addr).map(|p| p.name.clone()) {
                self.remembered = Some(name.clone());
                save_last_phone(&name, &addr.to_string());
            }
            if self.is_on() {
                self.broadcaster = None;
            }
            self.rebuild_menu();
        }
    }

    fn make_icon() -> Option<Icon> {
        // 22×22 template glyph: three ascending "signal" bars (black + alpha).
        let (w, h) = (22u32, 22u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        let bars = [(4u32, 14u32), (9, 9), (14, 5)]; // (x, top_y)
        for (bx, top) in bars {
            for y in top..18 {
                for x in bx..bx + 4 {
                    let i = ((y * w + x) * 4) as usize;
                    rgba[i + 3] = 255; // opaque black (RGB already 0)
                }
            }
        }
        Icon::from_rgba(rgba, w, h).ok()
    }
}

impl ApplicationHandler for Beam {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        if self.tray.is_some() {
            return; // build the tray exactly once
        }
        let mut builder = TrayIconBuilder::new().with_tooltip("beam — audio → phone");
        if let Some(icon) = Self::make_icon() {
            builder = builder.with_icon(icon).with_icon_as_template(true);
        }
        match builder.build() {
            Ok(tray) => {
                self.tray = Some(tray);
                self.poll_discovery();
                self.rebuild_menu();
            }
            Err(e) => eprintln!("beam: tray build failed: {e}"),
        }
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Drain menu clicks (delivered on a global channel by tray-icon).
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            self.on_menu(&ev.id);
        }
        // Refresh the phone list ~1×/s.
        if self.last_poll.elapsed() >= Duration::from_secs(1) {
            self.last_poll = Instant::now();
            if self.poll_discovery() {
                self.rebuild_menu();
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_secs(1)));
    }
}

// ── Remembered phone (shared idea with livehub, separate file) ───────────────

fn last_phone_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = std::path::Path::new(&home).join("Library/Application Support/beam");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("last_phone.txt"))
}

/// Load `(name, addr)`; addr only parsed if valid. Missing → `(None, None)`.
fn load_last_phone() -> (Option<String>, Option<SocketAddr>) {
    let Some(p) = last_phone_path() else { return (None, None) };
    let Ok(s) = std::fs::read_to_string(&p) else { return (None, None) };
    let (name, addr) = s.trim().split_once('\t').unwrap_or(("", s.trim()));
    let name = if name.is_empty() { None } else { Some(name.to_string()) };
    let addr = addr.parse::<SocketAddr>().ok();
    (name, addr)
}

fn save_last_phone(name: &str, addr: &str) {
    let _ = PORT_HINT; // (kept for future "name without port" fallback)
    if let Some(p) = last_phone_path() {
        let _ = std::fs::write(p, format!("{name}\t{addr}"));
    }
}
