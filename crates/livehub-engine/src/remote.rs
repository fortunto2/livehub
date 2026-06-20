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
pub const CAMPARAM_MAGIC: u32 = 0x5243_0400;

// Commands (Mac → phone).
pub const CMD_REC_START: u8 = 1;
pub const CMD_REC_STOP: u8 = 2;
pub const CMD_REC_TOGGLE: u8 = 3;
pub const CMD_PREVIEW_START: u8 = 4;
pub const CMD_PREVIEW_STOP: u8 = 5;
pub const CMD_PREVIEW_QUALITY: u8 = 6; // value = level 0=Low 1=Med 2=High (res + JPEG q)
pub const CMD_HEARTBEAT: u8 = 7; // both ways: liveness; phone→beam carries rx state in the port field

// Camera params (Mac → phone). Value is normalized 0..1 (toggles: 0.0 / 1.0); the
// phone maps it onto its own device ranges (ISO/shutter/EV/lens), so beam needs no
// knowledge of the camera's limits.
pub const CAM_MANUAL_EXP: u8 = 0;
pub const CAM_ISO: u8 = 1;
pub const CAM_SHUTTER: u8 = 2;
pub const CAM_EV: u8 = 3;
pub const CAM_MANUAL_FOCUS: u8 = 4;
pub const CAM_LENS: u8 = 5;
pub const CAM_AE_LOCK: u8 = 6;
pub const CAM_LENS_SELECT: u8 = 7; // value 0=ultra-wide, 1=wide, 2=tele
pub const CAM_FLIP: u8 = 8; // value 0=back, 1=front

/// Build the 9-byte camera-param packet: `magic · param u8 · value f32` (LE).
pub fn camparam_packet(param: u8, value: f32) -> [u8; 9] {
    let mut buf = [0u8; 9];
    buf[0..4].copy_from_slice(&CAMPARAM_MAGIC.to_le_bytes());
    buf[4] = param;
    buf[5..9].copy_from_slice(&value.to_le_bytes());
    buf
}

/// Build the 7-byte control packet.
pub fn control_packet(cmd: u8, preview_port: u16) -> [u8; 7] {
    let mut buf = [0u8; 7];
    buf[0..4].copy_from_slice(&CTRL_MAGIC.to_le_bytes());
    buf[4] = cmd;
    buf[5..7].copy_from_slice(&preview_port.to_le_bytes());
    buf
}

/// Send one control packet from a throwaway socket. Fine for one-offs; for
/// repeated sends use [`RemoteControl`] so the phone sees a STABLE source port
/// (one connection) instead of a new flow per packet.
pub fn send_control(to: SocketAddr, cmd: u8, preview_port: u16) -> std::io::Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.send_to(&control_packet(cmd, preview_port), to)?;
    Ok(())
}

/// A persistent control sender: one UDP socket reused for every command, so the
/// phone receives all control packets on a SINGLE connection (no per-packet flow
/// churn — important since the phone listens on every flow).
pub struct RemoteControl {
    sock: UdpSocket,
}

impl RemoteControl {
    pub fn new() -> std::io::Result<RemoteControl> {
        Ok(RemoteControl { sock: UdpSocket::bind("0.0.0.0:0")? })
    }

    pub fn send(&self, to: SocketAddr, cmd: u8, preview_port: u16) -> std::io::Result<()> {
        self.sock.send_to(&control_packet(cmd, preview_port), to)?;
        Ok(())
    }

    /// Send a camera parameter (normalized 0..1) to the phone over the same socket.
    pub fn send_cam(&self, to: SocketAddr, param: u8, value: f32) -> std::io::Result<()> {
        self.sock.send_to(&camparam_packet(param, value), to)?;
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Build one preview datagram: magic + seq + payload (matches the phone sender).
    fn preview_packet(seq: u32, payload: &[u8]) -> Vec<u8> {
        let mut p = Vec::with_capacity(8 + payload.len());
        p.extend_from_slice(&PREVIEW_MAGIC.to_le_bytes());
        p.extend_from_slice(&seq.to_le_bytes());
        p.extend_from_slice(payload);
        p
    }

    /// Poll a condition up to `ms`, sleeping between tries. Returns true if it held.
    fn wait_until(ms: u64, mut cond: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(ms) {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        cond()
    }

    #[test]
    fn control_packet_wire_format() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let to = sock.local_addr().unwrap();

        send_control(to, CMD_REC_TOGGLE, 54321).unwrap();
        let mut buf = [0u8; 16];
        let (n, _) = sock.recv_from(&mut buf).unwrap();
        assert_eq!(n, 7);
        assert_eq!(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]), CTRL_MAGIC);
        assert_eq!(buf[4], CMD_REC_TOGGLE);
        assert_eq!(u16::from_le_bytes([buf[5], buf[6]]), 54321);
    }

    #[test]
    fn camparam_packet_wire_format() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let to = sock.local_addr().unwrap();

        let rc = RemoteControl::new().unwrap();
        rc.send_cam(to, CAM_ISO, 0.5).unwrap();
        let mut buf = [0u8; 16];
        let (n, _) = sock.recv_from(&mut buf).unwrap();
        assert_eq!(n, 9);
        assert_eq!(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]), CAMPARAM_MAGIC);
        assert_eq!(buf[4], CAM_ISO);
        assert_eq!(f32::from_le_bytes([buf[5], buf[6], buf[7], buf[8]]), 0.5);
    }

    #[test]
    fn preview_keeps_only_latest_frame() {
        let rx = PreviewReceiver::start().unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let to: SocketAddr = format!("127.0.0.1:{}", rx.port()).parse().unwrap();

        // Send three distinct frames; the receiver must end on the LAST one.
        for (i, payload) in [b"AAAA".as_slice(), b"BBBB", b"CCCC"].iter().enumerate() {
            sender.send_to(&preview_packet(i as u32, payload), to).unwrap();
        }
        assert!(wait_until(1000, || rx.frame_seq() >= 3), "frames not received");
        assert_eq!(rx.latest_jpeg().as_deref(), Some(b"CCCC".as_slice()));
    }

    #[test]
    fn preview_survives_burst() {
        // Stability: 3000 frames back-to-back must not panic, must advance the seq,
        // and must keep only the last (bounded memory — no accumulation).
        let rx = PreviewReceiver::start().unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let to: SocketAddr = format!("127.0.0.1:{}", rx.port()).parse().unwrap();

        let last = 2999u32;
        let payload = vec![0xABu8; 4000]; // ~4 KB "JPEG"
        for i in 0..=last {
            // Best-effort; the OS UDP buffer may drop some — that's the point.
            let _ = sender.send_to(&preview_packet(i, &payload), to);
        }
        assert!(wait_until(2000, || rx.frame_seq() > 0), "no frames received");
        // Whatever arrived, the stored frame is exactly one payload-sized buffer.
        assert_eq!(rx.latest_jpeg().map(|v| v.len()), Some(4000));
    }

    #[test]
    fn non_preview_magic_is_ignored() {
        let rx = PreviewReceiver::start().unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let to: SocketAddr = format!("127.0.0.1:{}", rx.port()).parse().unwrap();
        // Audio-magic packet → must NOT be treated as a preview frame.
        let mut junk = 0x5243_0100u32.to_le_bytes().to_vec();
        junk.extend_from_slice(&[0u8; 64]);
        sender.send_to(&junk, to).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(rx.frame_seq(), 0);
        assert!(rx.latest_jpeg().is_none());
    }
}
