//! MultipeerConnectivity transport (Phase 1): spawn the `mp-bridge` Swift helper and
//! talk to it over pipes. The bridge runs an MCSession (AWDL peer-to-peer Wi-Fi) and
//! connects to reelcam with NO router/internet/SIM. Frames are `[u32 LE len][payload]`
//! both ways; the bridge's stderr carries connection status.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

pub struct Multipeer {
    child: Option<Child>,
    connected: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
    stdin_tx: Option<Sender<Vec<u8>>>,
    recv_rx: Option<Receiver<Vec<u8>>>,
}

impl Default for Multipeer {
    fn default() -> Self {
        Multipeer {
            child: None,
            connected: Arc::new(AtomicBool::new(false)),
            status: Arc::new(Mutex::new(String::new())),
            stdin_tx: None,
            recv_rx: None,
        }
    }
}

impl Multipeer {
    pub fn running(&self) -> bool {
        self.child.is_some()
    }
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
    #[allow(dead_code)] // kept for debugging the link state
    pub fn status(&self) -> String {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn start(&mut self) {
        if self.child.is_some() {
            return;
        }
        let mut child = match Command::new(bridge_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                *self.status.lock().unwrap() = format!("bridge failed: {e}");
                return;
            }
        };

        // stderr → status + connected flag.
        let connected = self.connected.clone();
        let status = self.status.clone();
        if let Some(err) = child.stderr.take() {
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    if let Some(rest) = line.strip_prefix("state ") {
                        connected.store(rest.ends_with(" connected"), Ordering::Relaxed);
                    }
                    if let Ok(mut s) = status.lock() {
                        *s = line;
                    }
                }
                connected.store(false, Ordering::Relaxed);
            });
        }

        // stdout → received frames.
        let (recv_tx, recv_rx) = channel::<Vec<u8>>();
        if let Some(mut out) = child.stdout.take() {
            std::thread::spawn(move || loop {
                let mut lenb = [0u8; 4];
                if out.read_exact(&mut lenb).is_err() {
                    break;
                }
                let len = u32::from_le_bytes(lenb) as usize;
                let mut payload = vec![0u8; len];
                if out.read_exact(&mut payload).is_err() {
                    break;
                }
                if recv_tx.send(payload).is_err() {
                    break;
                }
            });
        }

        // stdin ← frames to send.
        let (stdin_tx, stdin_rx) = channel::<Vec<u8>>();
        if let Some(mut inp) = child.stdin.take() {
            std::thread::spawn(move || {
                while let Ok(frame) = stdin_rx.recv() {
                    let len = (frame.len() as u32).to_le_bytes();
                    if inp.write_all(&len).is_err() || inp.write_all(&frame).is_err() {
                        break;
                    }
                    let _ = inp.flush();
                }
            });
        }

        self.child = Some(child);
        self.stdin_tx = Some(stdin_tx);
        self.recv_rx = Some(recv_rx);
    }

    pub fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
        }
        self.stdin_tx = None;
        self.recv_rx = None;
        self.connected.store(false, Ordering::Relaxed);
        *self.status.lock().unwrap() = String::new();
    }

    /// Send a payload to the phone over the MCSession. `reliable` = ordered/guaranteed
    /// (control) vs best-effort like UDP (audio/preview — never block on a slow link).
    /// The reliability is the first byte of the pipe frame; the bridge strips it.
    pub fn send(&self, data: Vec<u8>, reliable: bool) {
        if let Some(tx) = &self.stdin_tx {
            let mut frame = Vec::with_capacity(1 + data.len());
            frame.push(reliable as u8);
            frame.extend_from_slice(&data);
            let _ = tx.send(frame);
        }
    }

    /// Drain received frames (call each UI frame).
    pub fn drain(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if let Some(rx) = &self.recv_rx {
            while let Ok(f) = rx.try_recv() {
                out.push(f);
            }
        }
        out
    }
}

/// `mp-bridge` sits next to the beam executable (copied there by install-beam.sh).
fn bridge_path() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sib = dir.join("mp-bridge");
            if sib.exists() {
                return sib;
            }
        }
    }
    std::path::PathBuf::from("mp-bridge")
}
