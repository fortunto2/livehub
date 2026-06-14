//! Remote control + camera preview back-channel between a Mac (beam/livehub) and
//! the phone (reelcam), over the SAME local UDP path as the audio.
//!
//! - **Control (Mac → phone, to the phone's :9000):** a 7-byte packet
//!   `magic 0x52430200 · cmd u8 · preview_port u16` — start/stop recording, and
//!   start/stop the preview (carrying the UDP port the phone should send frames to).
//! - **Preview (phone → Mac):** `magic 0x52430300 · seq u32 · JPEG bytes` — one
//!   low-res JPEG frame per datagram (small enough to fit, lossy is fine).

use anyhow::Result;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

pub const CTRL_MAGIC: u32 = 0x5243_0200;
pub const PREVIEW_MAGIC: u32 = 0x5243_0300;

// Commands (Mac → phone).
pub const CMD_REC_START: u8 = 1;
pub const CMD_REC_STOP: u8 = 2;
pub const CMD_REC_TOGGLE: u8 = 3;
pub const CMD_PREVIEW_START: u8 = 4;
pub const CMD_PREVIEW_STOP: u8 = 5;

/// Send one control packet to the phone (its audio/control port, usually :9000).
/// `preview_port` is where the phone should send preview frames (0 if none).
pub fn send_control(to: SocketAddr, cmd: u8, preview_port: u16) -> std::io::Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    let mut buf = [0u8; 7];
    buf[0..4].copy_from_slice(&CTRL_MAGIC.to_le_bytes());
    buf[4] = cmd;
    buf[5..7].copy_from_slice(&preview_port.to_le_bytes());
    sock.send_to(&buf, to)?;
    Ok(())
}

/// Receives low-res JPEG preview frames from the phone on an ephemeral UDP port.
/// Hold it to keep receiving; drop it to stop. Poll `frame_seq()` for a change,
/// then `latest_jpeg()` for the bytes.
pub struct PreviewReceiver {
    latest: Arc<Mutex<Option<Vec<u8>>>>,
    seq: Arc<AtomicU32>,
    port: u16,
    running: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl PreviewReceiver {
    pub fn start() -> Result<PreviewReceiver> {
        let sock = UdpSocket::bind("0.0.0.0:0")?;
        sock.set_read_timeout(Some(std::time::Duration::from_millis(300)))?;
        let port = sock.local_addr()?.port();
        let latest = Arc::new(Mutex::new(None));
        let seq = Arc::new(AtomicU32::new(0));
        let running = Arc::new(AtomicBool::new(true));
        let (l, s, r) = (latest.clone(), seq.clone(), running.clone());
        let handle = std::thread::Builder::new()
            .name("preview-recv".into())
            .spawn(move || {
                let mut buf = vec![0u8; 65536];
                while r.load(Ordering::Relaxed) {
                    match sock.recv_from(&mut buf) {
                        Ok((n, _)) if n > 8 => {
                            let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                            if magic == PREVIEW_MAGIC {
                                let jpeg = buf[8..n].to_vec();
                                if let Ok(mut g) = l.lock() {
                                    *g = Some(jpeg);
                                }
                                s.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        _ => {} // timeout/short packet → loop, re-check `running`
                    }
                }
            })
            .ok();
        Ok(PreviewReceiver { latest, seq, port, running, handle })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
    pub fn frame_seq(&self) -> u32 {
        self.seq.load(Ordering::Relaxed)
    }
    pub fn latest_jpeg(&self) -> Option<Vec<u8>> {
        self.latest.lock().ok().and_then(|g| g.clone())
    }
}

impl Drop for PreviewReceiver {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
