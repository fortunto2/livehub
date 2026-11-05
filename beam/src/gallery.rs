//! Remote gallery: browse + pull recorded clips from the phone's file server over
//! WiFi (reelcam's `FileServer`, off-by-default + pairing-code-gated). Hand-rolled blocking
//! HTTP client on background threads (no extra deps); results flow back over a
//! channel and the egui UI polls them.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

/// Env var that pre-fills the pairing code (it is never written to disk by beam).
pub const CODE_ENV: &str = "BEAM_GALLERY_CODE";
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
    /// Per-session pairing code shown on the phone (Settings > Remote gallery).
    /// Memory only: never persisted, never logged.
    pub code: String,
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
            code: normalize_code(&std::env::var(CODE_ENV).unwrap_or_default()),
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
        if self.code.is_empty() {
            self.status = "enter the pairing code from Live2play Settings > Remote gallery".into();
            return;
        }
        let code = self.code.clone();
        self.thumbs.clear();
        self.status = "loading…".into();
        self.busy = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            match http_get(&host, PORT, &format!("/list?token={code}")) {
                Ok(body) => {
                    let items = parse_list(&body);
                    let names: Vec<String> = items.iter().map(|c| c.name.clone()).collect();
                    let _ = tx.send(Msg::List(items));
                    for name in names {
                        if let Ok(jpeg) =
                            http_get(&host, PORT, &format!("/thumb/{name}?token={code}"))
                        {
                            let _ = tx.send(Msg::Thumb(name, jpeg));
                        }
                    }
                    let _ = tx.send(Msg::Status(String::new())); // clear "loading"
                }
                Err(e) => {
                    let _ = tx.send(Msg::Status(explain(&e)));
                }
            }
        });
    }

    /// Download one clip to `dir`.
    pub fn download(&mut self, host: String, name: String) {
        let tx = self.tx.clone();
        let dir = self.dir.clone();
        let code = self.code.clone();
        self.busy = true;
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Status(format!("↓ {name}…")));
            match http_get(&host, PORT, &format!("/file/{name}?token={code}")) {
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
                Err(e) => let_status(&tx, explain(&e)),
            }
        });
    }

    /// Delete a clip from the PHONE (Photos). iOS prompts the user to confirm on the
    /// device, so the request blocks until they tap — then the clip drops off /list.
    pub fn delete_remote(&mut self, host: String, name: String) {
        let tx = self.tx.clone();
        let code = self.code.clone();
        self.busy = true;
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Status(format!(
                "deleting {name} — confirm on the phone…"
            )));
            match http_get(&host, PORT, &format!("/delete/{name}?token={code}")) {
                Ok(_) => let_status(&tx, format!("✓ deleted {name} on phone (hit refresh)")),
                Err(e) => let_status(&tx, explain(&e)),
            }
        });
    }

    /// Delete ALL clips from the phone (Photos) in one batch → a single confirmation
    /// on the device. Quick way to free space after pulling everything.
    pub fn delete_all_remote(&mut self, host: String) {
        let tx = self.tx.clone();
        let code = self.code.clone();
        self.busy = true;
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Status(
                "deleting ALL on phone — confirm on the device…".into(),
            ));
            match http_get(&host, PORT, &format!("/deleteall?token={code}")) {
                Ok(_) => let_status(&tx, "✓ phone cleared (hit refresh)".into()),
                Err(e) => let_status(&tx, explain(&e)),
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

/// Keep only what the phone can have issued (letters/digits), uppercased, so pasted
/// whitespace, dashes or URL-breaking characters never reach the request line.
pub fn normalize_code(raw: &str) -> String {
    raw.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Human message for a failed request. Never includes the request path (it holds the code).
fn explain(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => "✗ wrong or expired pairing code — turn the gallery on in Live2play Settings and copy the code".into(),
        std::io::ErrorKind::ConnectionRefused => "✗ gallery is off — turn it on in Live2play Settings > Remote gallery".into(),
        _ => format!("✗ {e}"),
    }
}

/// Minimal blocking HTTP GET. The server replies `Connection: close`, so read-to-end
/// captures the whole body. Returns the body (after the header) on `200`.
fn http_get(host: &str, port: u16, path: &str) -> std::io::Result<Vec<u8>> {
    let mut s = std::net::TcpStream::connect((host, port))?;
    s.set_read_timeout(Some(Duration::from_secs(60)))?;
    s.set_write_timeout(Some(Duration::from_secs(10)))?;
    // RFC 7230: an IPv6 literal in the Host header must be bracketed.
    let host_hdr = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let req =
        format!("GET {path} HTTP/1.1\r\nHost: {host_hdr}:{port}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes())?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf)?;
    let sep = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no header"))?;
    let head = std::str::from_utf8(&buf[..sep]).unwrap_or("");
    if !head.starts_with("HTTP/1.1 200") {
        let line = head.lines().next().unwrap_or("HTTP error");
        let kind = if line.contains(" 401") || line.contains(" 403") {
            std::io::ErrorKind::PermissionDenied
        } else {
            std::io::ErrorKind::Other
        };
        return Err(std::io::Error::new(kind, line.to_string()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// One-shot server: records the request line, answers with `status`.
    fn serve_once(status: &'static str) -> (u16, std::thread::JoinHandle<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut buf = [0u8; 2048];
            let n = c.read(&mut buf).unwrap();
            let _ = c.write_all(
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            );
            String::from_utf8_lossy(&buf[..n])
                .lines()
                .next()
                .unwrap_or("")
                .to_string()
        });
        (port, h)
    }

    #[test]
    fn request_carries_code_and_401_maps_to_message() {
        let (port, h) = serve_once("401 Unauthorized");
        let err = http_get("127.0.0.1", port, "/list?token=ABCD2345EFGH").unwrap_err();
        assert_eq!(h.join().unwrap(), "GET /list?token=ABCD2345EFGH HTTP/1.1");
        let msg = explain(&err);
        assert!(msg.contains("wrong or expired pairing code"));
        assert!(!msg.contains("ABCD2345EFGH"));
    }

    #[test]
    fn refused_maps_to_gallery_off() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let err = http_get("127.0.0.1", port, "/list?token=X").unwrap_err();
        assert!(explain(&err).contains("gallery is off"));
    }

    #[test]
    fn code_is_normalized() {
        assert_eq!(normalize_code(" abcd-2345 efgh\n"), "ABCD2345EFGH");
    }
}
