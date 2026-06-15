//! Remembered broadcast target — the reelcam phone's Bonjour name + ip:port text,
//! persisted so the same phone auto-re-targets across runs. SINGLE source of truth:
//! the livehub app and beam both call this (they write the SAME file, so they must
//! agree on the format — previously each had its own copy and they had diverged).

use std::path::PathBuf;

fn path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = std::path::Path::new(&home).join("Library/Application Support/livehub");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("last_phone.txt"))
}

/// Serialize `(name, addr)` → one line. `name` may be empty.
fn encode(name: &str, addr: &str) -> String {
    format!("{name}\t{addr}")
}

/// Parse a stored line → `(name, addr_text)`. Address is kept as text (it's
/// user-editable and may be empty before discovery resolves it).
fn decode(s: &str) -> (Option<String>, String) {
    let (name, addr) = s.trim().split_once('\t').unwrap_or(("", s.trim()));
    let name = if name.is_empty() { None } else { Some(name.to_string()) };
    (name, addr.to_string())
}

/// Load the remembered `(name, addr_text)`. Missing/empty → `(None, "")`.
pub fn load_last_phone() -> (Option<String>, String) {
    let Some(p) = path() else { return (None, String::new()) };
    match std::fs::read_to_string(&p) {
        Ok(s) => decode(&s),
        Err(_) => (None, String::new()),
    }
}

/// Remember `(name, addr)` for next launch. Best-effort; errors ignored.
pub fn save_last_phone(name: &str, addr: &str) {
    if let Some(p) = path() {
        let _ = std::fs::write(p, encode(name, addr));
    }
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};

    #[test]
    fn roundtrip_with_name() {
        let line = encode("reelcam", "172.20.10.2:9000");
        let (name, addr) = decode(&line);
        assert_eq!(name.as_deref(), Some("reelcam"));
        assert_eq!(addr, "172.20.10.2:9000");
    }

    #[test]
    fn empty_name_is_none() {
        let (name, addr) = decode(&encode("", "192.168.1.5:9000"));
        assert_eq!(name, None);
        assert_eq!(addr, "192.168.1.5:9000");
    }

    #[test]
    fn legacy_line_without_tab_is_address_only() {
        // A bare "ip:port" (no name, no tab) parses as address with no name.
        let (name, addr) = decode("172.20.10.2:9000");
        assert_eq!(name, None);
        assert_eq!(addr, "172.20.10.2:9000");
    }

    #[test]
    fn trims_whitespace() {
        let (name, addr) = decode("  phone\t10.0.0.1:9000\n");
        assert_eq!(name.as_deref(), Some("phone"));
        assert_eq!(addr, "10.0.0.1:9000");
    }
}
