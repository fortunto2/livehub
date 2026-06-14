//! Device-capture UDP audio broadcaster — capture ANY input device and stream it
//! to a phone over local WiFi. DECOUPLED from the DJ engine: the source is
//! whatever device you point it at, not necessarily livehub's master.
//!
//! Route into it via a loopback:
//!   - REAPER  → BlackHole → `broadcast --device BlackHole`
//!   - livehub → BlackHole (or its own out) → `broadcast --device BlackHole`
//!   - system audio → BlackHole;  or any hardware input directly.
//!
//! Field use needs NO internet: turn on the iPhone's Personal Hotspot, let the
//! Mac join it, point `--to` at the phone's hotspot IP. The audio callback only
//! pushes to a wait-free ring; a sender thread does the (syscall) UDP send.
//!
//! Wire format (little-endian), one packet per `FRAMES_PER_PACKET` stereo frames:
//!   magic u32 = 0x52431_00 ("RC" 0x01 0x00) · seq u64 · sample_rate u32 ·
//!   n_frames u16 · then interleaved-stereo f32 (L,R,L,R…)

use anyhow::{Context, Result};
use atomic_float::AtomicF32;
use cpal::traits::{DeviceTrait, StreamTrait};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::device::{best_input_config, pick_input};

/// 128 stereo frames per packet (~2.7 ms @ 48 kHz). Header(18) + 128*2*4 = 1042
/// bytes — under the 1500-byte Ethernet/WiFi MTU, so no IP fragmentation.
pub(crate) const FRAMES_PER_PACKET: usize = 128;
pub(crate) const MAGIC: u32 = 0x5243_0100;
pub(crate) const HEADER: usize = 4 + 8 + 4 + 2; // magic + seq + sr + n_frames

/// A running device→UDP audio broadcast. Hold it to keep broadcasting; drop it to
/// stop (RAII). The input callback only pushes to a wait-free ring; a sender
/// thread does the (syscall) UDP send. Used by the GUI toggle and the CLI.
pub struct Broadcaster {
    _stream: cpal::Stream, // input capture; kept alive so capture continues
    running: Arc<AtomicBool>,
    sender: Option<std::thread::JoinHandle<()>>,
    /// Smoothed 0..1 capture peak (fast attack / slow release) for a UI meter —
    /// lets you SEE that the source is actually carrying signal (e.g. confirm
    /// system audio is reaching BlackHole before you blame the network).
    level: Arc<AtomicF32>,
}

impl Broadcaster {
    /// Start capturing `device_hint`'s input and streaming it to `to`. Mono inputs
    /// are duplicated to stereo; multichannel inputs take channels 0–1.
    pub fn start(device_hint: &str, to: SocketAddr) -> Result<Broadcaster> {
        let dev = pick_input(device_hint)?;
        let cfg = best_input_config(&dev)?;
        let in_ch = (cfg.channels() as usize).max(1);
        let sr = cfg.sample_rate().0;
        eprintln!(
            "broadcast: capturing '{}' {in_ch}ch @ {sr}Hz → {to}",
            dev.name().unwrap_or_default()
        );

        let (mut prod, cons) = rtrb::RingBuffer::<f32>::new(sr as usize * 2); // ~1 s stereo
        let level = Arc::new(AtomicF32::new(0.0));
        let cb_level = level.clone();
        let stream_cfg: cpal::StreamConfig = cfg.clone().into();
        let stream = dev
            .build_input_stream(
                &stream_cfg,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let frames = data.len() / in_ch;
                    let mut peak = 0.0f32;
                    for f in 0..frames {
                        let base = f * in_ch;
                        let l = data.get(base).copied().unwrap_or(0.0);
                        let r =
                            if in_ch >= 2 { data.get(base + 1).copied().unwrap_or(0.0) } else { l };
                        peak = peak.max(l.abs()).max(r.abs());
                        let _ = prod.push(l); // drop on overflow — a live monitor can skip
                        let _ = prod.push(r);
                    }
                    // Peak meter: fast attack, slow release.
                    let prev = cb_level.load(Ordering::Relaxed);
                    let v = if peak > prev { peak } else { prev * 0.85 };
                    cb_level.store(v.min(1.0), Ordering::Relaxed);
                },
                |e| eprintln!("broadcast input error: {e}"),
                None,
            )
            .context("build_input_stream")?;
        stream.play().context("input stream play")?;

        let running = Arc::new(AtomicBool::new(true));
        let sender = {
            let running = running.clone();
            std::thread::Builder::new()
                .name("broadcast-sender".into())
                .spawn(move || sender_loop(cons, sr, to, running))
                .ok()
        };
        Ok(Broadcaster { _stream: stream, running, sender, level })
    }

    /// Smoothed 0..1 capture level for a UI meter.
    pub fn level(&self) -> f32 {
        self.level.load(Ordering::Relaxed)
    }
}

impl Drop for Broadcaster {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(h) = self.sender.take() {
            let _ = h.join();
        }
        // `_stream` drops here → capture stops.
    }
}

/// Drain the ring, pack `FRAMES_PER_PACKET` stereo frames per UDP packet, send.
fn sender_loop(mut cons: rtrb::Consumer<f32>, sr: u32, to: SocketAddr, running: Arc<AtomicBool>) {
    let sock = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("broadcast: bind failed: {e}");
            return;
        }
    };
    let mut packet = vec![0u8; HEADER + FRAMES_PER_PACKET * 2 * 4];
    packet[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    packet[12..16].copy_from_slice(&sr.to_le_bytes());
    packet[16..18].copy_from_slice(&(FRAMES_PER_PACKET as u16).to_le_bytes());

    let mut seq: u64 = 0;
    let mut filled = 0usize;
    while running.load(Ordering::Relaxed) {
        let mut got_any = false;
        while let Ok(s) = cons.pop() {
            got_any = true;
            let off = HEADER + filled * 4;
            packet[off..off + 4].copy_from_slice(&s.to_le_bytes());
            filled += 1;
            if filled == FRAMES_PER_PACKET * 2 {
                packet[4..12].copy_from_slice(&seq.to_le_bytes());
                let _ = sock.send_to(&packet, to);
                seq = seq.wrapping_add(1);
                filled = 0;
            }
        }
        if !got_any {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

/// Persistent, GATED sender for the engine's INTERNAL master tap (livehub's own
/// mix → phone, no loopback device). Unlike `sender_loop` (one fixed target,
/// lives with a `Broadcaster`), this runs for the engine's whole lifetime and is
/// toggled live by `on` + `target`: when off it just drains+discards the ring so
/// it never backs up. Same wire format. Exits when `alive` goes false.
pub(crate) fn master_sender_loop(
    mut cons: rtrb::Consumer<f32>,
    sr: u32,
    target: Arc<Mutex<Option<SocketAddr>>>,
    on: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
) {
    let sock = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("master bcast: bind failed: {e}");
            return;
        }
    };
    let mut packet = vec![0u8; HEADER + FRAMES_PER_PACKET * 2 * 4];
    packet[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    packet[12..16].copy_from_slice(&sr.to_le_bytes());
    packet[16..18].copy_from_slice(&(FRAMES_PER_PACKET as u16).to_le_bytes());

    let mut seq: u64 = 0;
    let mut filled = 0usize;
    while alive.load(Ordering::Relaxed) {
        // Read the destination once per drain pass; `None` ⇒ off ⇒ discard.
        let dest = if on.load(Ordering::Relaxed) {
            target.lock().ok().and_then(|g| *g)
        } else {
            None
        };
        let mut got_any = false;
        while let Ok(s) = cons.pop() {
            got_any = true;
            if dest.is_none() {
                filled = 0; // off: drop samples so the ring can't overflow
                continue;
            }
            let off = HEADER + filled * 4;
            packet[off..off + 4].copy_from_slice(&s.to_le_bytes());
            filled += 1;
            if filled == FRAMES_PER_PACKET * 2 {
                packet[4..12].copy_from_slice(&seq.to_le_bytes());
                if let Some(to) = dest {
                    let _ = sock.send_to(&packet, to);
                }
                seq = seq.wrapping_add(1);
                filled = 0;
            }
        }
        if !got_any {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

/// CLI: capture `device_hint` → UDP to `to` until the process is killed (Ctrl-C).
pub fn run_broadcast(device_hint: &str, to: SocketAddr) -> Result<()> {
    let _b = Broadcaster::start(device_hint, to)?;
    eprintln!("broadcast: streaming (Ctrl-C to stop)");
    loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}
