//! Remote gallery: browse + pull recorded clips from the phone's file server over
//! WiFi (reelcam's `FileServer`, off-by-default + token-gated). Hand-rolled blocking
//! HTTP client on background threads (no extra deps); results flow back over a
//! channel and the egui UI polls them.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

/// Must match reelcam `FileServer.token`.
pub const TOKEN: &str = "rcm-7f3a9c2e8b1d4056";
pub const PORT: u16 = 9010;

#[derive(Clone)]
pub struct ClipMeta {
    pub name: String,
    pub size: u64,
    pub date: String,
    pub duration: f32,
}

enum Msg {
    List(Vec<ClipMeta>),
    Thumb(String, Vec<u8>),
    Status(String),
}

pub struct Gallery {
    pub open: bool,
    pub items: Vec<ClipMeta>,
    pub thumbs: HashMap<String, egui::TextureHandle>,
    pub status: String,
    pub busy: bool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    pub dir: PathBuf,
}

impl Default for Gallery {
    fn default() -> Self {
        let (tx, rx) = channel();
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        Gallery {
            open: false,
            items: Vec::new(),
            thumbs: HashMap::new(),
            status: String::new(),
            busy: false,
            tx,
            rx,
            dir: PathBuf::from(home).join("Movies/Reelcam"),
        }
    }
}

impl Gallery {
    /// (Re)load the clip list + thumbnails from `host` (the phone's IP).
    pub fn refresh(&mut self, host: String) {
        self.items.clear();
        self.thumbs.clear();
        self.status = "loading…".into();
        self.busy = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            match http_get(&host, PORT, &format!("/list?token={TOKEN}")) {
                Ok(body) => {
                    let items = parse_list(&body);
                    let names: Vec<String> = items.iter().map(|c| c.name.clone()).collect();
                    let _ = tx.send(Msg::List(items));
                    for name in names {
                        if let Ok(jpeg) =
                            http_get(&host, PORT, &format!("/thumb/{name}?token={TOKEN}"))
                        {
                            let _ = tx.send(Msg::Thumb(name, jpeg));
                        }
                    }
                    let _ = tx.send(Msg::Status(String::new())); // clear "loading"
                }
                Err(e) => {
                    let _ = tx.send(Msg::Status(format!("✗ {e} (gallery off on the phone?)")));
                }
            }
        });
    }

    /// Download one clip to `dir`.
    pub fn download(&mut self, host: String, name: String) {
        let tx = self.tx.clone();
        let dir = self.dir.clone();
        self.busy = true;
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Status(format!("↓ {name}…")));
            match http_get(&host, PORT, &format!("/file/{name}?token={TOKEN}")) {
                Ok(bytes) => {
                    std::fs::create_dir_all(&dir).ok();
                    let path = dir.join(&name);
                    match std::fs::write(&path, &bytes) {
                        Ok(_) => {
                            let_status(&tx, format!("✓ saved → {}", path.display()));
                            open_path(&path); // auto-play once it's down
                        }
                        Err(e) => let_status(&tx, format!("✗ write: {e}")),
                    }
                }
                Err(e) => let_status(&tx, format!("✗ {e}")),
            }
        });
    }

    /// Delete a clip from the PHONE (Photos). iOS prompts the user to confirm on the
    /// device, so the request blocks until they tap — then the clip drops off /list.
    pub fn delete_remote(&mut self, host: String, name: String) {
        let tx = self.tx.clone();
        self.busy = true;
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Status(format!("deleting {name} — confirm on the phone…")));
            match http_get(&host, PORT, &format!("/delete/{name}?token={TOKEN}")) {
                Ok(_) => let_status(&tx, format!("✓ deleted {name} on phone (hit refresh)")),
                Err(e) => let_status(&tx, format!("✗ {e}")),
            }
        });
    }

    /// Drain background results into the UI state (call each frame).
    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::List(items) => {
                    self.status = format!("{} clips", items.len());
                    self.items = items;
                    self.busy = false;
                }
                Msg::Thumb(name, jpeg) => {
                    if let Ok(img) = image::load_from_memory(&jpeg) {
                        let rgba = img.to_rgba8();
                        let (w, h) = (rgba.width() as usize, rgba.height() as usize);
                        let color = egui::ColorImage::from_rgba_unmultiplied([w, h], rgba.as_raw());
                        let tex = ctx.load_texture(&name, color, egui::TextureOptions::LINEAR);
                        self.thumbs.insert(name, tex);
                    }
                }
                Msg::Status(s) => {
                    if !s.is_empty() {
                        self.status = s;
                    }
                    self.busy = false;
                }
            }
        }
    }
}

fn let_status(tx: &Sender<Msg>, s: String) {
    let _ = tx.send(Msg::Status(s));
}

/// Open a downloaded clip in the system player (QuickTime) — egui can't play video
/// inline, and `open` gives full video + audio in its own window.
pub fn open_path(path: &std::path::Path) {
    let _ = std::process::Command::new("open").arg(path).spawn();
}

/// Minimal blocking HTTP GET. The server replies `Connection: close`, so read-to-end
/// captures the whole body. Returns the body (after the header) on `200`.
fn http_get(host: &str, port: u16, path: &str) -> std::io::Result<Vec<u8>> {
    let mut s = std::net::TcpStream::connect((host, port))?;
    s.set_read_timeout(Some(Duration::from_secs(60)))?;
    s.set_write_timeout(Some(Duration::from_secs(10)))?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes())?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf)?;
    let sep = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no header"))?;
    let head = std::str::from_utf8(&buf[..sep]).unwrap_or("");
    if !head.starts_with("HTTP/1.1 200") {
        let code = head.lines().next().unwrap_or("HTTP error");
        return Err(std::io::Error::new(std::io::ErrorKind::Other, code.to_string()));
    }
    Ok(buf[sep + 4..].to_vec())
}

/// Parse the `/list` TSV: name⇥size⇥epoch⇥duration per line.
fn parse_list(body: &[u8]) -> Vec<ClipMeta> {
    String::from_utf8_lossy(body)
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 4 || f[0].is_empty() {
                return None;
            }
            Some(ClipMeta {
                name: f[0].to_string(),
                size: f[1].parse().unwrap_or(0),
                date: f[2].to_string(),
                duration: f[3].parse().unwrap_or(0.0),
            })
        })
        .collect()
}
