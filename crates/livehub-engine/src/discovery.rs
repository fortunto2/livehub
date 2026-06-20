//! mDNS/Bonjour discovery of reelcam phones on the local network, so the broadcast
//! target auto-fills — no manual ip:port. reelcam advertises `_reelcam._udp`.
//!
//! Backed by the SYSTEM resolver (`/usr/bin/dns-sd` → mDNSResponder), NOT a pure-Rust
//! mDNS stack: on this Mac the pure-Rust `mdns-sd` returns 0 peers (multicast is gated
//! / awdl0 interferes) while the system daemon — which owns the multicast socket and
//! the permission — reliably sees the phone. We spawn `dns-sd -B` to browse, `dns-sd -L`
//! to get host:port, then `getaddrinfo` (std `to_socket_addrs`, which resolves `.local`
//! via mDNSResponder on macOS) for the address — preferring the WiFi-LAN IP over awdl0.
//! Browsing runs on a background thread; the GUI reads `peers()`.

use anyhow::Result;
use std::io::{BufRead, BufReader};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// The `dns-sd` CLI wants the bare service type (NO trailing dot): `dns-sd -B
// _reelcam._udp local.`. Its OUTPUT, though, prints the type WITH a trailing dot
// (`… local. _reelcam._udp. reelcam`) — so parse the dotted form.
const SERVICE_ARG: &str = "_reelcam._udp";
const SERVICE_DOT: &str = "_reelcam._udp.";

/// Pick the best address: a real WiFi-LAN IPv4 (192.168/10/172) over a 169.254
/// link-local or an awdl0 IPv6 — so audio rides WiFi, not the JITTERY AWDL link (the
/// phone advertises on BOTH when the Multipeer link is up).
fn pick_addr(addrs: &[IpAddr]) -> Option<IpAddr> {
    addrs
        .iter()
        .find(|ip| matches!(ip, IpAddr::V4(v4) if v4.is_private()))
        .or_else(|| {
            addrs
                .iter()
                .find(|ip| matches!(ip, IpAddr::V4(v4) if !v4.is_link_local() && !v4.is_loopback()))
        })
        .or_else(|| addrs.iter().find(|ip| ip.is_ipv4()))
        .or_else(|| addrs.first())
        .copied()
}

/// A discovered reelcam phone.
#[derive(Clone)]
pub struct Peer {
    pub name: String,     // instance name (e.g. "reelcam")
    pub addr: SocketAddr, // resolved ip:port to broadcast to
}

/// Live list of discovered phones. Start once; poll `peers()`. Dropping it kills the
/// `dns-sd` browse subprocess.
pub struct Discovery {
    peers: Arc<Mutex<Vec<Peer>>>,
    browse: Child,
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.browse.kill();
        let _ = self.browse.wait();
    }
}

impl Discovery {
    pub fn start() -> Result<Discovery> {
        let mut browse = Command::new("dns-sd")
            .args(["-B", SERVICE_ARG, "local."])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .spawn()?;
        let stdout = browse
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("dns-sd: no stdout"))?;
        let peers = Arc::new(Mutex::new(Vec::<Peer>::new()));
        let store = peers.clone();
        std::thread::Builder::new()
            .name("dns-sd-browse".into())
            .spawn(move || {
                // Lines look like:
                //   15:48:40  Add  2  14 local. _reelcam._udp. reelcam
                //   15:48:41  Rmv  0  14 local. _reelcam._udp. reelcam
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    let added = parts.contains(&"Add");
                    let removed = parts.contains(&"Rmv");
                    if !(added || removed) {
                        continue;
                    }
                    let inst = match parts.iter().position(|&p| p == SERVICE_DOT) {
                        Some(ti) if ti + 1 < parts.len() => parts[ti + 1..].join(" "),
                        _ => continue,
                    };
                    if removed {
                        store.lock().unwrap().retain(|p| p.name != inst);
                        continue;
                    }
                    if let Some(addr) = resolve_instance(&inst) {
                        let mut g = store.lock().unwrap();
                        // upsert by name (the phone's IP can change between scans)
                        if let Some(p) = g.iter_mut().find(|p| p.name == inst) {
                            p.addr = addr;
                        } else {
                            g.push(Peer { name: inst, addr });
                        }
                    }
                }
            })?;
        Ok(Discovery { peers, browse })
    }

    /// Currently-discovered phones.
    pub fn peers(&self) -> Vec<Peer> {
        self.peers.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Re-query the network from scratch (fresh browse subprocess). Use when the phone
    /// dropped off the list (app backgrounded / network blip) and hasn't re-appeared.
    pub fn refresh(&mut self) {
        if let Ok(fresh) = Discovery::start() {
            let _ = self.browse.kill();
            let _ = self.browse.wait();
            *self = fresh;
        } else {
            self.peers.lock().map(|mut g| g.clear()).ok();
        }
    }
}

/// Resolve an instance → ip:port via `dns-sd -L` (host:port) + `getaddrinfo` (address).
/// `dns-sd -L` runs forever, so read with a deadline then kill it.
fn resolve_instance(inst: &str) -> Option<SocketAddr> {
    let mut child = Command::new("dns-sd")
        .args(["-L", inst, SERVICE_ARG, "local."])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    // Parse "... can be reached at Rusts-iPhone.local.:9000 (interface 14) ..."
    let mut host_port: Option<(String, u16)> = None;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        match rx.recv_timeout(Duration::from_millis(300)) {
            Ok(line) => {
                if let Some(idx) = line.find("can be reached at ") {
                    let rest = &line[idx + "can be reached at ".len()..];
                    let token = rest.split_whitespace().next().unwrap_or("");
                    if let Some(cpos) = token.rfind(':') {
                        let host = token[..cpos].trim_end_matches('.').to_string();
                        if let Ok(port) = token[cpos + 1..].parse::<u16>() {
                            host_port = Some((host, port));
                            break;
                        }
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    drop(rx); // unblock the reader thread (its send fails → loop breaks)
    let _ = reader.join();

    let (host, port) = host_port?;
    // getaddrinfo: on macOS this resolves `.local` via mDNSResponder. May return both an
    // IPv4 (WiFi LAN) and an IPv6 link-local — pick_addr prefers the WiFi-LAN IPv4.
    let addrs: Vec<IpAddr> = (host.as_str(), port)
        .to_socket_addrs()
        .ok()?
        .map(|sa| sa.ip())
        .collect();
    let ip = pick_addr(&addrs)?;
    Some(SocketAddr::new(ip, port))
}
