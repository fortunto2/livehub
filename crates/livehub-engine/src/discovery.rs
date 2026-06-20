//! mDNS/Bonjour discovery of reelcam phones on the local network, so the
//! broadcast target auto-fills — no manual ip:port. reelcam advertises
//! `_reelcam._udp`; we browse + resolve. Works on the iPhone hotspot too (mDNS
//! is link-local). Browsing runs on a background thread; the GUI reads `peers()`.

use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

const SERVICE: &str = "_reelcam._udp.local.";

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

/// Live list of discovered phones. Start once; poll `peers()`. Dropping it stops
/// the mDNS daemon.
pub struct Discovery {
    peers: Arc<Mutex<Vec<Peer>>>,
    _daemon: ServiceDaemon,
}

impl Discovery {
    pub fn start() -> Result<Discovery> {
        let daemon = ServiceDaemon::new()?;
        let rx = daemon.browse(SERVICE)?;
        let peers = Arc::new(Mutex::new(Vec::<Peer>::new()));
        let store = peers.clone();
        std::thread::Builder::new()
            .name("mdns-browse".into())
            .spawn(move || {
                while let Ok(event) = rx.recv() {
                    match event {
                        ServiceEvent::ServiceResolved(info) => {
                            let port = info.get_port();
                            // mdns-sd 0.20 yields `ScopedIp`; convert to plain `IpAddr`,
                            // then prefer the WiFi-LAN address over awdl0 (see pick_addr).
                            let addrs: Vec<IpAddr> =
                                info.get_addresses().iter().map(|a| a.to_ip_addr()).collect();
                            if let Some(ip) = pick_addr(&addrs) {
                                let addr = SocketAddr::new(ip, port);
                                let name = info
                                    .get_fullname()
                                    .split('.')
                                    .next()
                                    .unwrap_or("reelcam")
                                    .to_string();
                                let mut g = store.lock().unwrap();
                                if !g.iter().any(|p| p.addr == addr) {
                                    g.push(Peer { name, addr });
                                }
                            }
                        }
                        ServiceEvent::ServiceRemoved(_, fullname) => {
                            let short = fullname.split('.').next().unwrap_or("").to_string();
                            store.lock().unwrap().retain(|p| p.name != short);
                        }
                        _ => {}
                    }
                }
            })?;
        Ok(Discovery { peers, _daemon: daemon })
    }

    /// Currently-discovered phones.
    pub fn peers(&self) -> Vec<Peer> {
        self.peers.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Re-query the network from scratch (fresh daemon + browse). Use when the phone
    /// dropped off the list (app backgrounded / network blip) and hasn't re-appeared.
    /// Replaces this `Discovery` in place; the old daemon is dropped (stops).
    pub fn refresh(&mut self) {
        if let Ok(fresh) = Discovery::start() {
            *self = fresh;
        } else {
            self.peers.lock().map(|mut g| g.clear()).ok();
        }
    }
}
