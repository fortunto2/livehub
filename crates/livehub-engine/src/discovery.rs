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
use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// The `dns-sd` CLI wants the bare service type (NO trailing dot): `dns-sd -B
// _reelcam._udp local.`. Its OUTPUT, though, prints the type WITH a trailing dot
// (`… local. _reelcam._udp. reelcam`) — so parse the dotted form.
const SERVICE_ARG: &str = "_reelcam._udp";
const SERVICE_DOT: &str = "_reelcam._udp.";

/// Rank candidate addresses, best first: a real WiFi-LAN IPv4 (192.168/10/172), then
/// other IPv4, then global IPv6, then link-local IPv6 — so audio prefers plain WiFi
/// IPv4 (not the JITTERY AWDL v6 link the phone also advertises when Multipeer is up).
/// ALL candidates are kept: on an IPv6-only carrier hotspot (T-Mobile 464XLAT,
/// measured 2026-09-24) the phone still ADVERTISES 172.20.10.1 but never services
/// IPv4 — the v4 is dead and only v6 carries packets. beam fails over down this list
/// when the phone's heartbeat goes silent, so a dead first choice heals itself.
/// Ranks full `SocketAddr`s (NOT bare `IpAddr`s) because a link-local IPv6 target is
/// only routable with its scope_id, which lives in `SocketAddrV6`.
fn rank_addrs(addrs: &[SocketAddr]) -> Vec<SocketAddr> {
    let score = |sa: &SocketAddr| match sa.ip() {
        IpAddr::V4(v4) if v4.is_private() => 0,
        IpAddr::V4(v4) if !v4.is_link_local() && !v4.is_loopback() => 1,
        IpAddr::V4(_) => 2,
        IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) != 0xfe80 => 3, // global v6
        IpAddr::V6(_) => 4,                                          // link-local v6
    };
    let mut ranked: Vec<SocketAddr> = addrs.to_vec();
    ranked.sort_by_key(score);
    // Vec::dedup only removes ADJACENT duplicates; a duplicate separated by
    // another equal-score address would survive and burn a failover slot.
    let mut seen = std::collections::HashSet::new();
    ranked.retain(|a| seen.insert(*a));
    ranked
}

/// Is this address actually THIS Mac (a stale mDNS record resolving to one of our own
/// interface IPs)? After a DHCP reshuffle the phone's old `.local` A-record can linger in
/// mDNSResponder pointing at an IP the Mac itself later took — beam would then "target the
/// phone" by talking to itself: no preview comes back, the link reads LOST, and the
/// broadcast goes nowhere. Detect it dependency-free: a UDP `connect()` to the addr makes
/// the kernel pick the egress source IP; if the route to `addr` leaves via an interface
/// whose own IP == `addr.ip()` (i.e. it's local), source == dest → it's us. No packet is
/// sent (connect just sets the default peer).
pub fn is_self_addr(addr: &SocketAddr) -> bool {
    let bind = if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    UdpSocket::bind(bind)
        .and_then(|s| {
            s.connect(addr)?;
            Ok(s.local_addr()?.ip() == addr.ip())
        })
        .unwrap_or(false)
}

/// A discovered reelcam phone.
#[derive(Clone)]
pub struct Peer {
    pub name: String,     // instance name (e.g. "reelcam")
    pub addr: SocketAddr, // best resolved ip:port to broadcast to (= addrs[0])
    /// ALL resolved candidates, best first (v4 → global v6 → link-local v6). beam
    /// walks down this list when the current target goes heartbeat-silent.
    pub addrs: Vec<SocketAddr>,
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
                    if let Some(addrs) = resolve_instance(&inst) {
                        let addr = addrs[0];
                        let mut g = store.lock().unwrap();
                        // upsert by name (the phone's IP can change between scans)
                        if let Some(p) = g.iter_mut().find(|p| p.name == inst) {
                            p.addr = addr;
                            p.addrs = addrs;
                        } else {
                            g.push(Peer { name: inst, addr, addrs });
                        }
                    }
                }
            })?;
        Ok(Discovery { peers, browse })
    }

    /// Currently-discovered phones. Drops any ADDRESS that is THIS Mac (a stale
    /// self-pointing mDNS record — see `is_self_addr`), and any peer with no real
    /// address left, so auto-target never picks us.
    pub fn peers(&self) -> Vec<Peer> {
        self.peers
            .lock()
            .map(|g| {
                g.iter()
                    .filter_map(|p| {
                        let addrs: Vec<SocketAddr> =
                            p.addrs.iter().filter(|a| !is_self_addr(a)).copied().collect();
                        let addr = *addrs.first()?;
                        Some(Peer { name: p.name.clone(), addr, addrs })
                    })
                    .collect()
            })
            .unwrap_or_default()
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

/// Resolve an instance → ranked ip:port candidates via `dns-sd -L` (host:port) +
/// `getaddrinfo` (addresses). `dns-sd -L` runs forever, so read with a deadline then
/// kill it. Returns a non-empty list, best first.
fn resolve_instance(inst: &str) -> Option<Vec<SocketAddr>> {
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
    // getaddrinfo: on macOS this resolves `.local` via mDNSResponder. May return an
    // IPv4 (WiFi LAN), a global IPv6 and a scoped link-local IPv6 — keep them ALL,
    // ranked, so beam can fail over when the preferred one turns out dead.
    let addrs: Vec<SocketAddr> = (host.as_str(), port).to_socket_addrs().ok()?.collect();
    let ranked = rank_addrs(&addrs);
    (!ranked.is_empty()).then_some(ranked)
}
