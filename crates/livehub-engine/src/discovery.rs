//! mDNS/Bonjour discovery of reelcam phones on the local network, so the
//! broadcast target auto-fills — no manual ip:port. reelcam advertises
//! `_reelcam._udp`; we browse + resolve. Works on the iPhone hotspot too (mDNS
//! is link-local). Browsing runs on a background thread; the GUI reads `peers()`.

use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

const SERVICE: &str = "_reelcam._udp.local.";

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
                            // Prefer an IPv4 address; fall back to any. mdns-sd 0.20
                            // yields `ScopedIp`; convert to plain `IpAddr`.
                            let ip = info
                                .get_addresses()
                                .iter()
                                .find(|a| a.is_ipv4())
                                .or_else(|| info.get_addresses().iter().next())
                                .map(|a| a.to_ip_addr());
                            if let Some(ip) = ip {
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
}
