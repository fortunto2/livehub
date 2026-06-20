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
use std::io::Write;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};

use crate::wav::WavWriter;

use crate::device::{
    best_input_config, best_input_config_at, best_output_config_at, pick_input, pick_output,
};

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
    sr: u32, // the ACTUAL capture rate (device-chosen) — what the phone receives

    /// Smoothed 0..1 capture peak (fast attack / slow release) for a UI meter —
    /// lets you SEE that the source is actually carrying signal (e.g. confirm
    /// system audio is reaching BlackHole before you blame the network).
    level: Arc<AtomicF32>,
    /// A/V-sync test: when set, the next captured block is replaced by a sharp
    /// full-scale CLICK. Paired with a screen flash, the offset between them in the
    /// phone's recording = the audio-vs-video latency. See `pulse()`.
    pulse: Arc<AtomicBool>,
    /// A/V-sync test: when set, the captured device audio is muted (broadcast
    /// SILENCE) so the only transient is the click — otherwise music/voice on the
    /// source pollutes click detection and ruins the measurement.
    silent: Arc<AtomicBool>,
    /// Each outgoing audio packet, mirrored here so beam can forward it over the
    /// Multipeer (AWDL) link (Direct). Taken out once by a dedicated forward thread so
    /// packets stream smoothly (the UI loop batched them into bursts → crackle).
    tap_rx: Option<Receiver<Vec<u8>>>,
    /// Gate for the tap — only clone+forward packets when Direct transport is on.
    tap_on: Arc<AtomicBool>,
    /// Local-monitor output (hear the source on the Mac); silent until `monitor_on`.
    _out_stream: Option<cpal::Stream>,
    monitor_on: Arc<AtomicBool>,
    /// Local backup recording → clean WAV (for post-merge); gated by `rec_on`.
    rec_on: Arc<AtomicBool>,
    rec_cmd_tx: Sender<RecCmd>,
    rec_writer: Option<std::thread::JoinHandle<()>>,
}

impl Broadcaster {
    /// Start capturing `device_hint`'s input and streaming it to `to`. Mono inputs
    /// are duplicated to stereo; multichannel inputs take channels 0–1.
    /// `sample_rate` of 0 = AUTO: use the device's own rate (preferring 48 kHz). A
    /// nonzero value asks the device to run at that rate (CoreAudio resamples or we
    /// fall back to the device max if it can't). The phone follows whatever actually
    /// flows (it's in the packet header), so AUTO is the clean default.
    pub fn start(device_hint: &str, to: SocketAddr, sample_rate: u32) -> Result<Broadcaster> {
        let dev = pick_input(device_hint)?;
        let cfg = if sample_rate == 0 {
            best_input_config(&dev)?
        } else {
            best_input_config_at(&dev, sample_rate)?
        };
        let in_ch = (cfg.channels() as usize).max(1);
        let sr = cfg.sample_rate().0;
        eprintln!(
            "broadcast: capturing '{}' {in_ch}ch @ {sr}Hz → {to}",
            dev.name().unwrap_or_default()
        );

        let (mut prod, cons) = rtrb::RingBuffer::<f32>::new(sr as usize * 2); // ~1 s stereo
        // Local monitor: a second ring fed only while listening, drained by an output
        // stream on the Mac's speakers/headphones — so you HEAR the source (e.g. REAPER
        // via BlackHole) that's otherwise going only to the phone.
        let (mut mon_prod, mut mon_cons) = rtrb::RingBuffer::<f32>::new(sr as usize); // ~0.5 s stereo
        let monitor_on = Arc::new(AtomicBool::new(false));
        let cb_monitor = monitor_on.clone();
        // Local backup recording: a third ring filled only while recording, drained by
        // a writer thread → a clean 16-bit WAV (no network artifacts) for post-merge.
        let (mut rec_prod, rec_cons) = rtrb::RingBuffer::<f32>::new(sr as usize * 2);
        let rec_on = Arc::new(AtomicBool::new(false));
        let cb_rec = rec_on.clone();
        let level = Arc::new(AtomicF32::new(0.0));
        let cb_level = level.clone();
        let pulse = Arc::new(AtomicBool::new(false));
        let cb_pulse = pulse.clone();
        let silent = Arc::new(AtomicBool::new(false));
        let cb_silent = silent.clone();
        let stream_cfg: cpal::StreamConfig = cfg.clone().into();
        let stream = dev
            .build_input_stream(
                &stream_cfg,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let frames = data.len() / in_ch;
                    // A/V-sync click: replace this block with a sharp impulse.
                    let click = cb_pulse.swap(false, Ordering::Relaxed);
                    let mute = cb_silent.load(Ordering::Relaxed);
                    let mut peak = 0.0f32;
                    for f in 0..frames {
                        let base = f * in_ch;
                        let (l, r) = if click {
                            // ~1 ms square at full scale → a sharp, easy-to-detect click.
                            let v = if f < (sr as usize / 1000).max(8) {
                                if f % 8 < 4 { 1.0 } else { -1.0 }
                            } else {
                                0.0
                            };
                            (v, v)
                        } else if mute {
                            (0.0, 0.0) // sync-test silence: only the click survives
                        } else {
                            let l = data.get(base).copied().unwrap_or(0.0);
                            let r = if in_ch >= 2 {
                                data.get(base + 1).copied().unwrap_or(0.0)
                            } else {
                                l
                            };
                            (l, r)
                        };
                        peak = peak.max(l.abs()).max(r.abs());
                        let _ = prod.push(l); // drop on overflow — a live monitor can skip
                        let _ = prod.push(r);
                        if cb_monitor.load(Ordering::Relaxed) {
                            let _ = mon_prod.push(l); // → local speakers (only while listening)
                            let _ = mon_prod.push(r);
                        }
                        if cb_rec.load(Ordering::Relaxed) {
                            let _ = rec_prod.push(l); // → local WAV (only while recording)
                            let _ = rec_prod.push(r);
                        }
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

        // Monitor output on the Mac's default output, at the source rate (no drift). It
        // always drains the monitor ring (empty = silence when not listening). Best-effort
        // — if the default output can't be opened, listening is simply unavailable.
        let out_stream = pick_output("").ok().and_then(|od| {
            let oc = best_output_config_at(&od, sr).ok()?;
            let out_ch = oc.channels().max(1) as usize;
            let ocfg: cpal::StreamConfig = oc.into();
            let s = od
                .build_output_stream(
                    &ocfg,
                    move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        let mut i = 0;
                        while i + out_ch <= out.len() {
                            let l = mon_cons.pop().unwrap_or(0.0);
                            let r = mon_cons.pop().unwrap_or(0.0);
                            for c in 0..out_ch {
                                out[i + c] = if c % 2 == 0 { l } else { r };
                            }
                            i += out_ch;
                        }
                    },
                    |e| eprintln!("monitor output error: {e}"),
                    None,
                )
                .ok()?;
            s.play().ok()?;
            Some(s)
        });

        // Local-recording writer thread (drains rec ring → WAV files on command).
        let (rec_cmd_tx, rec_cmd_rx) = channel::<RecCmd>();
        let rec_writer = std::thread::Builder::new()
            .name("broadcast-rec".into())
            .spawn(move || record_writer(rec_cons, rec_cmd_rx, sr))
            .ok();

        let running = Arc::new(AtomicBool::new(true));
        let (tap_tx, tap_rx) = sync_channel::<Vec<u8>>(64); // bounded — drop on backpressure
        let tap_on = Arc::new(AtomicBool::new(false));
        let sender = {
            let running = running.clone();
            let tap_on = tap_on.clone();
            std::thread::Builder::new()
                .name("broadcast-sender".into())
                .spawn(move || sender_loop(cons, sr, to, running, Some((tap_tx, tap_on))))
                .ok()
        };
        Ok(Broadcaster {
            _stream: stream,
            _out_stream: out_stream,
            running,
            sender,
            sr,
            level,
            pulse,
            silent,
            tap_rx: Some(tap_rx),
            tap_on,
            monitor_on,
            rec_on,
            rec_cmd_tx,
            rec_writer,
        })
    }

    /// Take the audio-packet receiver (once) — beam drains it on a dedicated thread and
    /// streams each packet over the Multipeer link as it's produced (smooth, like the
    /// UDP path). Gated by `set_tap` (the broadcaster only fills it in Direct mode).
    pub fn take_tap(&mut self) -> Option<Receiver<Vec<u8>>> {
        self.tap_rx.take()
    }

    /// Enable/disable the Multipeer tap (set true while Direct transport is selected).
    pub fn set_tap(&self, on: bool) {
        self.tap_on.store(on, Ordering::Relaxed);
    }

    /// Listen to the broadcast source on the Mac's own output (speakers/headphones) —
    /// for when the source (e.g. REAPER → BlackHole) is otherwise inaudible locally.
    pub fn set_monitor(&self, on: bool) {
        self.monitor_on.store(on, Ordering::Relaxed);
    }
    pub fn monitoring(&self) -> bool {
        self.monitor_on.load(Ordering::Relaxed)
    }

    /// Start recording the clean source to `path` (a fresh 16-bit WAV). Synced to the
    /// phone's record so the audio can be merged onto the video in post.
    pub fn record_start(&self, path: PathBuf) {
        let _ = self.rec_cmd_tx.send(RecCmd::Start(path));
        self.rec_on.store(true, Ordering::Relaxed);
    }
    pub fn record_stop(&self) {
        self.rec_on.store(false, Ordering::Relaxed);
        let _ = self.rec_cmd_tx.send(RecCmd::Stop);
    }

    /// Fire one A/V-sync CLICK into the broadcast on the next captured block. Pair
    /// it with a screen flash the phone films; the click↔flash offset in the
    /// recording is the audio-vs-video latency.
    pub fn pulse(&self) {
        self.pulse.store(true, Ordering::Relaxed);
    }

    /// Mute the captured device audio (broadcast silence) so a sync-test click is
    /// the only transient — keep music/voice off the measurement.
    pub fn set_silent(&self, on: bool) {
        self.silent.store(on, Ordering::Relaxed);
    }

    /// Smoothed 0..1 capture level for a UI meter.
    /// The actual capture/broadcast sample rate (device-chosen) — what the phone gets.
    pub fn sample_rate(&self) -> u32 {
        self.sr
    }

    pub fn level(&self) -> f32 {
        self.level.load(Ordering::Relaxed)
    }
}

impl Drop for Broadcaster {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // Finalize any open WAV + stop the recorder thread cleanly.
        self.rec_on.store(false, Ordering::Relaxed);
        let _ = self.rec_cmd_tx.send(RecCmd::Quit);
        if let Some(h) = self.rec_writer.take() {
            let _ = h.join();
        }
        if let Some(h) = self.sender.take() {
            let _ = h.join();
        }
        // `_stream` drops here → capture stops.
    }
}

/// A lightweight input-level meter — opens the device just to measure its peak (no
/// UDP, no broadcast). Lets beam show the source level BEFORE you hit ▶ so you can
/// confirm signal is reaching the device. Drop it before starting a `Broadcaster` on
/// the same device (one capture stream at a time).
pub struct LevelMonitor {
    _stream: cpal::Stream,
    level: Arc<AtomicF32>,
}

impl LevelMonitor {
    pub fn start(device_hint: &str) -> Result<LevelMonitor> {
        let dev = pick_input(device_hint)?;
        let cfg = best_input_config(&dev)?;
        let stream_cfg: cpal::StreamConfig = cfg.into();
        let level = Arc::new(AtomicF32::new(0.0));
        let cb_level = level.clone();
        let stream = dev
            .build_input_stream(
                &stream_cfg,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let mut peak = 0.0f32;
                    for &s in data {
                        peak = peak.max(s.abs());
                    }
                    let prev = cb_level.load(Ordering::Relaxed);
                    let v = if peak > prev { peak } else { prev * 0.85 };
                    cb_level.store(v.min(1.0), Ordering::Relaxed);
                },
                |_| {},
                None,
            )
            .context("build_input_stream (level monitor)")?;
        stream.play()?;
        Ok(LevelMonitor { _stream: stream, level })
    }

    pub fn level(&self) -> f32 {
        self.level.load(Ordering::Relaxed)
    }
}

/// Drain `cons`, pack `FRAMES_PER_PACKET` stereo frames per UDP packet, and send to
/// the address from `dest()` (read once per drain pass). `dest()` returning `None`
/// ⇒ a gated sender that's currently off: samples are drained+discarded so the ring
/// can't overflow. Runs until `alive` goes false. The SINGLE place the audio wire
/// header is written — both the fixed and gated senders below funnel through here.
fn pump(
    mut cons: rtrb::Consumer<f32>,
    sr: u32,
    alive: Arc<AtomicBool>,
    mut dest: impl FnMut() -> Option<SocketAddr>,
    // forward each packet here when the gate is on (for the Multipeer link)
    tap: Option<(SyncSender<Vec<u8>>, Arc<AtomicBool>)>,
) {
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
    while alive.load(Ordering::Relaxed) {
        let to = dest();
        let mut got_any = false;
        while let Ok(s) = cons.pop() {
            got_any = true;
            let Some(addr) = to else {
                filled = 0; // gated off → drop so the ring can't overflow
                continue;
            };
            let off = HEADER + filled * 4;
            packet[off..off + 4].copy_from_slice(&s.to_le_bytes());
            filled += 1;
            if filled == FRAMES_PER_PACKET * 2 {
                packet[4..12].copy_from_slice(&seq.to_le_bytes());
                let _ = sock.send_to(&packet, addr);
                // Only clone+forward to the Multipeer tap when Direct transport wants it
                // (else WiFi mode would clone every packet for a channel nobody drains).
                if let Some((t, on)) = &tap {
                    if on.load(Ordering::Relaxed) {
                        let _ = t.try_send(packet.clone()); // drop on full — audio may skip
                    }
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

/// Fixed-target sender (a `Broadcaster`'s capture → one phone). `tap` also forwards
/// every packet to the Multipeer link (when Direct transport is wanted).
fn sender_loop(
    cons: rtrb::Consumer<f32>,
    sr: u32,
    to: SocketAddr,
    running: Arc<AtomicBool>,
    tap: Option<(SyncSender<Vec<u8>>, Arc<AtomicBool>)>,
) {
    pump(cons, sr, running, move || Some(to), tap);
}

/// Persistent, GATED sender for the engine's INTERNAL master tap (livehub's own mix
/// → phone, no loopback device). Lives for the engine's whole lifetime; toggled live
/// by `on` + `target` (off ⇒ drains+discards). Exits when `alive` goes false.
pub(crate) fn master_sender_loop(
    cons: rtrb::Consumer<f32>,
    sr: u32,
    target: Arc<Mutex<Option<SocketAddr>>>,
    on: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
) {
    pump(
        cons,
        sr,
        alive,
        move || {
            if on.load(Ordering::Relaxed) {
                target.lock().ok().and_then(|g| *g)
            } else {
                None
            }
        },
        None,
    );
}

/// Commands to the local-recording writer thread.
enum RecCmd {
    Start(PathBuf),
    Stop,
    Quit,
}

/// Where recorded audio lands: AAC via an ffmpeg pipe (`.m4a` — directly mergeable onto
/// the phone's H.264/HEVC video with `-c copy`, no conversion), or a plain WAV fallback
/// when ffmpeg is absent.
enum RecSink {
    Aac { child: Child, stdin: ChildStdin },
    Wav(WavWriter),
}

fn ffmpeg_path() -> Option<PathBuf> {
    ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
}

fn open_sink(path: &PathBuf, sr: u32) -> Option<RecSink> {
    let want_aac = path.extension().map(|e| e == "m4a").unwrap_or(false);
    if want_aac {
        if let Some(ff) = ffmpeg_path() {
            if let Ok(mut child) = Command::new(ff)
                .args(["-y", "-f", "s16le", "-ar"])
                .arg(sr.to_string())
                .args(["-ac", "2", "-i", "pipe:0", "-c:a", "aac", "-b:a", "256k"])
                .arg(path)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                if let Some(stdin) = child.stdin.take() {
                    return Some(RecSink::Aac { child, stdin });
                }
            }
        }
    }
    // No ffmpeg (or a non-m4a path) → WAV fallback.
    WavWriter::create(&path.with_extension("wav"), sr, 2)
        .ok()
        .map(RecSink::Wav)
}

fn close_sink(sink: Option<RecSink>) {
    match sink {
        Some(RecSink::Aac { mut child, stdin }) => {
            drop(stdin); // EOF → ffmpeg finalizes the .m4a
            let _ = child.wait();
        }
        Some(RecSink::Wav(w)) => {
            let _ = w.finalize();
        }
        None => {}
    }
}

/// Drains the broadcaster's record ring (interleaved-stereo f32) and writes it to the
/// current sink (AAC/.m4a via ffmpeg, or WAV). Lives for the broadcaster's lifetime.
fn record_writer(mut cons: rtrb::Consumer<f32>, cmd_rx: Receiver<RecCmd>, sr: u32) {
    let mut sink: Option<RecSink> = None;
    let mut samples: Vec<i16> = Vec::with_capacity(8192);
    loop {
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                RecCmd::Start(path) => {
                    close_sink(sink.take());
                    sink = open_sink(&path, sr);
                }
                RecCmd::Stop => close_sink(sink.take()),
                RecCmd::Quit => {
                    close_sink(sink.take());
                    return;
                }
            }
        }
        samples.clear();
        while let Ok(s) = cons.pop() {
            samples.push((s.clamp(-1.0, 1.0) * 32767.0) as i16);
        }
        if !samples.is_empty() {
            match &mut sink {
                Some(RecSink::Aac { stdin, .. }) => {
                    let mut bytes = Vec::with_capacity(samples.len() * 2);
                    for &v in &samples {
                        bytes.extend_from_slice(&v.to_le_bytes());
                    }
                    let _ = stdin.write_all(&bytes);
                }
                Some(RecSink::Wav(w)) => {
                    let _ = w.write(&samples);
                }
                None => {}
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pump_emits_one_well_formed_audio_packet() {
        let recv = UdpSocket::bind("127.0.0.1:0").unwrap();
        recv.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        let to = recv.local_addr().unwrap();

        let (mut prod, cons) = rtrb::RingBuffer::<f32>::new(FRAMES_PER_PACKET * 2 + 16);
        for i in 0..(FRAMES_PER_PACKET * 2) {
            let _ = prod.push(i as f32 / 1000.0); // a known ramp
        }

        let alive = Arc::new(AtomicBool::new(true));
        let sr = 48_000u32;
        let handle = {
            let alive = alive.clone();
            std::thread::spawn(move || pump(cons, sr, alive, move || Some(to), None))
        };

        let mut buf = [0u8; 2048];
        let (n, _) = recv.recv_from(&mut buf).expect("packet");
        alive.store(false, Ordering::Relaxed);
        let _ = handle.join();

        assert_eq!(n, HEADER + FRAMES_PER_PACKET * 2 * 4);
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), MAGIC);
        assert_eq!(u32::from_le_bytes(buf[12..16].try_into().unwrap()), sr);
        assert_eq!(u16::from_le_bytes(buf[16..18].try_into().unwrap()), FRAMES_PER_PACKET as u16);
        // Samples land in order, little-endian f32.
        let s0 = f32::from_le_bytes(buf[HEADER..HEADER + 4].try_into().unwrap());
        let s1 = f32::from_le_bytes(buf[HEADER + 4..HEADER + 8].try_into().unwrap());
        assert_eq!(s0, 0.0);
        assert_eq!(s1, 1.0 / 1000.0);
    }

    #[test]
    fn pump_with_no_destination_drops_and_does_not_send() {
        // Gated off (dest() → None): it must drain the ring without sending.
        let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
        probe.set_read_timeout(Some(std::time::Duration::from_millis(200))).unwrap();
        let to = probe.local_addr().unwrap();

        let (mut prod, cons) = rtrb::RingBuffer::<f32>::new(FRAMES_PER_PACKET * 2 + 16);
        for _ in 0..(FRAMES_PER_PACKET * 2) {
            let _ = prod.push(1.0);
        }
        let alive = Arc::new(AtomicBool::new(true));
        let handle = {
            let alive = alive.clone();
            // Destination is always None → nothing should reach `to`.
            std::thread::spawn(move || pump(cons, 48_000, alive, move || None::<SocketAddr>, None))
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        alive.store(false, Ordering::Relaxed);
        let _ = handle.join();
        let _ = to; // probe never bound to receive from this socket; assert silence:
        let mut buf = [0u8; 64];
        assert!(probe.recv_from(&mut buf).is_err(), "gated-off pump must not send");
    }
}

/// CLI: capture `device_hint` → UDP to `to` until the process is killed (Ctrl-C).
pub fn run_broadcast(device_hint: &str, to: SocketAddr) -> Result<()> {
    let _b = Broadcaster::start(device_hint, to, 48_000)?;
    eprintln!("broadcast: streaming (Ctrl-C to stop)");
    loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// One sample of the diagnostic test tone: a 250 ms 1 kHz START MARKER at t=0, then
/// an 80 ms 1 kHz beep at every second boundary, silence between. Lets an analyzer
/// find where the recorded audio actually begins (start gap) and whether every
/// second's beep is present + 1 s apart (dropouts / drift).
fn test_sample(t: f64) -> f32 {
    use std::f64::consts::TAU;
    let into_second = t - t.floor();
    let beep_len = if t < 1.0 { 0.25 } else { 0.08 };
    if into_second < beep_len {
        ((TAU * 1000.0 * t).sin() * 0.6) as f32
    } else {
        0.0
    }
}

/// CLI/diagnostic: broadcast the marker test tone to `to` for `secs`, real-time
/// paced, same wire format as a live stream. With `record`, remote-toggles the
/// phone's recording around it (start before, stop after) so a whole take is
/// captured hands-free for analysis.
pub fn run_broadcast_test(to: SocketAddr, secs: u64, record: bool) -> Result<()> {
    let ctl = if record { crate::remote::RemoteControl::new().ok() } else { None };
    if let Some(c) = &ctl {
        let _ = c.send(to, crate::remote::CMD_REC_TOGGLE, 0);
        std::thread::sleep(std::time::Duration::from_millis(500)); // let recording spin up
    }
    eprintln!("broadcast-test: {secs}s marker tone → {to}{}", if record { " (recording)" } else { "" });

    let sock = UdpSocket::bind("0.0.0.0:0")?;
    let sr = 48_000u32;
    let mut packet = vec![0u8; HEADER + FRAMES_PER_PACKET * 2 * 4];
    packet[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    packet[12..16].copy_from_slice(&sr.to_le_bytes());
    packet[16..18].copy_from_slice(&(FRAMES_PER_PACKET as u16).to_le_bytes());

    let total = sr as usize * secs as usize;
    let mut frame = 0usize;
    let mut seq = 0u64;
    let start = std::time::Instant::now();
    while frame < total {
        for i in 0..FRAMES_PER_PACKET {
            let s = test_sample((frame + i) as f64 / sr as f64).to_le_bytes();
            let off = HEADER + i * 8;
            packet[off..off + 4].copy_from_slice(&s);
            packet[off + 4..off + 8].copy_from_slice(&s);
        }
        packet[4..12].copy_from_slice(&seq.to_le_bytes());
        let _ = sock.send_to(&packet, to);
        seq += 1;
        frame += FRAMES_PER_PACKET;
        let target = start + std::time::Duration::from_secs_f64(frame as f64 / sr as f64);
        if let Some(d) = target.checked_duration_since(std::time::Instant::now()) {
            std::thread::sleep(d);
        }
    }
    if let Some(c) = &ctl {
        let _ = c.send(to, crate::remote::CMD_REC_TOGGLE, 0);
        eprintln!("broadcast-test: stopped recording");
    }
    Ok(())
}
