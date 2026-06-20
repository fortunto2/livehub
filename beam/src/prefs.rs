//! Persisted beam preferences (source device, channel pair, master gain, monitor,
//! transport) so the rig comes back the same across launches. Separate from the shared
//! `last_phone` file. Plain `key=value` lines — no serde dep, forgiving on parse.

use std::path::PathBuf;

#[derive(Clone, PartialEq, Default)]
pub struct Prefs {
    pub device: String,
    pub channel: usize,
    pub gain: f32,
    pub source_listen: bool,
    pub transport: u8, // 0=Auto, 1=WiFi, 2=Direct
}

fn path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = std::path::Path::new(&home).join("Library/Application Support/livehub");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("beam_prefs.txt"))
}

/// Load persisted prefs. Missing/unparseable fields fall back to these defaults —
/// transport defaults to WiFi (1): AWDL/Direct time-slices the WiFi radio, so plain
/// WiFi is the right default; Direct is opt-in for the field.
pub fn load() -> Prefs {
    let mut p = Prefs { gain: 1.0, transport: 1, ..Prefs::default() };
    let Some(path) = path() else { return p };
    let Ok(s) = std::fs::read_to_string(&path) else { return p };
    for line in s.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim() {
            "device" => p.device = v.to_string(),
            "channel" => p.channel = v.parse().unwrap_or(0),
            "gain" => p.gain = v.parse().unwrap_or(1.0),
            "source_listen" => p.source_listen = v == "1",
            "transport" => p.transport = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    p
}

/// Persist prefs. Best-effort; errors ignored.
pub fn save(p: &Prefs) {
    let Some(path) = path() else { return };
    let body = format!(
        "device={}\nchannel={}\ngain={}\nsource_listen={}\ntransport={}\n",
        p.device,
        p.channel,
        p.gain,
        u8::from(p.source_listen),
        p.transport,
    );
    let _ = std::fs::write(path, body);
}
