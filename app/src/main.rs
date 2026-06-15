//! livehub CLI — for M0 this is a headless audio test harness (list devices,
//! mic→master passthrough, channel sweep). The egui surface arrives in M1.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod gui;
mod import;
mod video;

#[derive(Parser)]
#[command(name = "livehub", version, about = "Live DJ/VJ hub")]
struct Cli {
    /// GUI output/input device (substring match).
    #[arg(long, default_value = "Universal Audio")]
    device: String,
    /// Optional aux stereo input "L,R" (e.g. "10,11" on the "Mixx Rig" aggregate)
    /// to mix other apps' audio in via BlackHole. Off by default.
    #[arg(long)]
    aux: Option<String>,
    /// Auto-load this media file into deck A on GUI launch.
    #[arg(long)]
    open: Option<std::path::PathBuf>,
    /// Auto-load this media file into deck B on GUI launch.
    #[arg(long)]
    open_b: Option<std::path::PathBuf>,
    /// Start playing immediately after the auto-loaded file(s) finish decoding.
    #[arg(long)]
    play: bool,
    /// With no subcommand, launch the GUI.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

/// Parse an `"L,R"` channel pair (e.g. `"10,11"`).
fn parse_aux(s: Option<&str>) -> Option<(usize, usize)> {
    let (l, r) = s?.split_once(',')?;
    Some((l.trim().parse().ok()?, r.trim().parse().ok()?))
}

#[derive(Subcommand)]
enum Cmd {
    /// List input + output audio devices with channel counts.
    ListDevices,
    /// Mic → master passthrough on one (aggregate) device. Ctrl-C to stop.
    Passthrough {
        /// Device name substring (case-insensitive).
        #[arg(long, default_value = "Universal Audio")]
        device: String,
        /// Mic input channel index.
        #[arg(long, default_value_t = 0)]
        mic_ch: usize,
        /// Master output left channel index.
        #[arg(long, default_value_t = 0)]
        master_l: usize,
        /// Master output right channel index.
        #[arg(long, default_value_t = 1)]
        master_r: usize,
    },
    /// Sweep a 440 Hz tone across each output channel to identify routing.
    Sweep {
        #[arg(long, default_value = "Universal Audio")]
        device: String,
    },
    /// Capture an input device and UDP-stream it to a phone (reelcam live mode).
    /// Point `--device` at a loopback (BlackHole carrying REAPER / livehub /
    /// system audio) or a hardware input. `--to` is the phone's ip:port — over
    /// the iPhone's Personal Hotspot in the field, no internet needed.
    Broadcast {
        /// Input device name substring (e.g. "BlackHole").
        #[arg(long, default_value = "BlackHole")]
        device: String,
        /// Phone address `ip:port` (e.g. 172.20.10.2:9000).
        #[arg(long)]
        to: String,
    },
    /// Probe whether the phone HANDLES remote commands: ask it for a camera
    /// preview frame and see if one comes back. ✓ = latest build (remote-record
    /// works); ✗ = old build or unreachable. Gives the Mac side visibility.
    ProbePhone {
        /// Phone address `ip:port`.
        #[arg(long)]
        to: String,
    },
    /// Broadcast a DIAGNOSTIC marker tone (250ms start beep + 1 beep/sec) to the
    /// phone for `--secs`, optionally remote-recording it. Analyze the resulting
    /// clip with `tools/audiocheck.py` to find audio start-gaps / dropouts / drift.
    BroadcastTest {
        /// Phone address `ip:port`.
        #[arg(long)]
        to: String,
        /// How long to broadcast (seconds).
        #[arg(long, default_value_t = 12)]
        secs: u64,
        /// Remote-toggle the phone's recording around the broadcast.
        #[arg(long)]
        record: bool,
    },
    /// Load a file and play the deck mixed with the live mic. Ctrl-C to stop.
    Play {
        /// Audio file to play on the deck.
        file: String,
        #[arg(long, default_value = "Universal Audio")]
        device: String,
        /// Loop the track.
        #[arg(long)]
        looped: bool,
    },
    /// Headless self-test: decode + analyze a media file and probe its video.
    /// No GUI, no audio device — prints a report. For autonomous testing.
    Selftest {
        /// Media file (audio or video) to analyze.
        file: String,
    },
    /// Headless A/V pacing measurement on a video: cadence (smoothness) + sync
    /// stats, by running the real video worker against a synthetic playhead.
    SelftestPace {
        /// Video file to measure.
        file: String,
    },
    /// Headless seek-stress test: scripted forward/backward seeks, measuring the
    /// recovery latency of each (a backward-seek stall shows up here).
    SelftestSeek {
        /// Video file to measure.
        file: String,
    },
    /// Headless loop-geometry test: for each loop size, print the region it would
    /// engage (length in beats & seconds vs nominal) and assert it's exactly k
    /// beats and that halving the size halves the length. No GUI, no audio device.
    SelftestLoops {
        /// Media file (its beat grid is analyzed) to test.
        file: String,
    },
    /// List avfoundation video capture devices (cameras) usable as a deck source.
    ListCameras,
    /// Headless self-test of the static-image source: decode + scale an image and
    /// report its dimensions / generation / slot state. For autonomous testing.
    SelftestImage {
        /// Image file (png/jpg/webp/…) to load.
        file: String,
    },
    /// Headless self-test of the beat visualizer: analyze a file's beatgrid, run
    /// the real render worker against a synthetic playhead, and assert the pulse
    /// peaks on the beat and frames are produced. For autonomous testing.
    SelftestViz {
        /// Media file (its beat grid drives the visualizer) to test.
        file: String,
    },
    /// List installed SuperDuper*.clap bundles usable as the master-bus insert.
    ListFx,
    /// Headless CLAP e2e: load a SuperDuper effect as the master insert at
    /// 48k/512, push a nonzero sine through it, and assert the output differs
    /// from the input (proves audio-ports + processing are wired). No audio
    /// device, no GUI. `plugin` is a basename substring (e.g. "Filter").
    SelftestFx {
        /// Plugin basename substring to match among installed SuperDuper*.clap.
        plugin: String,
    },
    /// Headless beatmatch test: decode two media files, print each detected BPM,
    /// the `sync_rate(bpmB, bpmA)` that retempos B onto A, and the resulting
    /// matched BPM (bpmB × rate). Asserts the math headlessly. No GUI, no audio.
    SelftestSync {
        /// File A (the target tempo — B is retempo'd onto it).
        file_a: String,
        /// File B (gets retempo'd to match A).
        file_b: String,
    },
    /// Download a deck's media from a URL via yt-dlp (≤720p mp4, audio+video) into
    /// the import scratch dir and print the resulting file path. Mirrors the GUI's
    /// 🔗 import worker, so the download is verifiable headlessly.
    Import {
        /// Media URL (YouTube, etc.) to download.
        url: String,
    },
    /// OPTIONAL karaoke split: run demucs (`--two-stems=vocals`) on a file and
    /// print the instrumental (`no_vocals.wav`) path. Degrades gracefully — if
    /// demucs is not installed it prints the install hint and exits 0.
    Karaoke {
        /// Audio/video file to split into vocals + instrumental.
        file: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        None => gui::run(
            &cli.device,
            parse_aux(cli.aux.as_deref()),
            cli.open,
            cli.open_b,
            cli.play,
        ),
        Some(Cmd::ListDevices) => livehub_engine::list_devices(),
        Some(Cmd::Passthrough { device, mic_ch, master_l, master_r }) => {
            livehub_engine::run_passthrough(&device, mic_ch, master_l, master_r)
        }
        Some(Cmd::Sweep { device }) => livehub_engine::run_sweep(&device),
        Some(Cmd::Broadcast { device, to }) => {
            let addr: std::net::SocketAddr = to
                .parse()
                .map_err(|e| anyhow::anyhow!("--to must be ip:port ({to}): {e}"))?;
            livehub_engine::run_broadcast(&device, addr)
        }
        Some(Cmd::BroadcastTest { to, secs, record }) => {
            let addr: std::net::SocketAddr = to
                .parse()
                .map_err(|e| anyhow::anyhow!("--to must be ip:port ({to}): {e}"))?;
            livehub_engine::run_broadcast_test(addr, secs, record)
        }
        Some(Cmd::ProbePhone { to }) => {
            let addr: std::net::SocketAddr = to
                .parse()
                .map_err(|e| anyhow::anyhow!("--to must be ip:port ({to}): {e}"))?;
            let rx = livehub_engine::PreviewReceiver::start()?;
            eprintln!("probe: asking the phone for a preview frame on port {}…", rx.port());
            livehub_engine::send_control(addr, livehub_engine::CMD_PREVIEW_START, rx.port())?;
            let mut got = false;
            for _ in 0..50 {
                if rx.frame_seq() > 0 {
                    got = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let _ = livehub_engine::send_control(addr, livehub_engine::CMD_PREVIEW_STOP, 0);
            if got {
                println!("✓ phone RESPONDS — preview frame received. Latest build; remote-record works.");
            } else {
                println!("✗ NO response in 5s — phone is on an OLD build (no remote handling) or unreachable.");
                println!("  (If the audio counter on the phone DOES move during a broadcast, the receiver");
                println!("   works but remote-control isn't in that build → record manually.)");
            }
            Ok(())
        }
        Some(Cmd::Play { file, device, looped }) => play(&device, &file, looped),
        Some(Cmd::Selftest { file }) => selftest(&file),
        Some(Cmd::SelftestPace { file }) => {
            println!("== livehub pace test: {file} ==");
            println!("{}", video::pace_test(std::path::Path::new(&file)));
            Ok(())
        }
        Some(Cmd::SelftestSeek { file }) => {
            println!("== livehub seek test: {file} ==");
            println!("{}", video::seek_test(std::path::Path::new(&file)));
            Ok(())
        }
        Some(Cmd::SelftestLoops { file }) => selftest_loops(&file),
        Some(Cmd::ListCameras) => {
            println!("== livehub cameras ==");
            let cams = video::CameraDeck::list_cameras();
            if cams.is_empty() {
                println!("  (none)");
            } else {
                for c in cams {
                    println!("  {c}");
                }
            }
            Ok(())
        }
        Some(Cmd::SelftestImage { file }) => {
            println!("== livehub image test: {file} ==");
            println!("{}", video::selftest_image(std::path::Path::new(&file)));
            Ok(())
        }
        Some(Cmd::SelftestViz { file }) => {
            println!("== livehub visualizer test: {file} ==");
            let t = livehub_engine::decode_track(std::path::Path::new(&file), 48_000)?;
            println!("  grid : bpm={:.1}, {} beats", t.bpm, t.beats.len());
            println!("{}", video::selftest_viz(&t.beats, t.bpm));
            Ok(())
        }
        Some(Cmd::ListFx) => {
            println!("== livehub master-FX plugins ==");
            let fx = livehub_engine::Engine::list_fx();
            if fx.is_empty() {
                println!("  (none — no SuperDuper*.clap in ~/Library/Audio/Plug-Ins/CLAP/)");
            } else {
                for p in fx {
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                    println!("  {name}");
                }
            }
            Ok(())
        }
        Some(Cmd::SelftestFx { plugin }) => selftest_fx(&plugin),
        Some(Cmd::SelftestSync { file_a, file_b }) => selftest_sync(&file_a, &file_b),
        Some(Cmd::Import { url }) => import_cmd(&url),
        Some(Cmd::Karaoke { file }) => karaoke_cmd(&file),
    }
}

/// Headless URL import: download `url` to the import scratch dir via yt-dlp and
/// print the resulting mp4 path (or the error). Same `import::yt_dlp_download` the
/// GUI worker calls — so the download is testable without a window.
fn import_cmd(url: &str) -> Result<()> {
    println!("== livehub import: {url} ==");
    match import::yt_dlp_download(url, &import::import_dir()) {
        Ok(path) => println!("  downloaded: {}", path.display()),
        Err(e) => println!("  import FAILED: {e:#}"),
    }
    Ok(())
}

/// Headless karaoke split. If demucs is absent, print the install hint and exit
/// cleanly (the feature is OPTIONAL). Otherwise split via the SAME
/// `import::demucs_split` the GUI worker calls and print the instrumental path.
fn karaoke_cmd(file: &str) -> Result<()> {
    println!("== livehub karaoke: {file} ==");
    if !import::demucs_available() {
        println!("  demucs not installed — uv tool install demucs");
        return Ok(());
    }
    match import::demucs_split(std::path::Path::new(file), &import::import_dir()) {
        Ok(path) => println!("  instrumental: {}", path.display()),
        Err(e) => println!("  karaoke FAILED: {e:#}"),
    }
    Ok(())
}

/// Headless beatmatch (SYNC) test. Decodes both files for their detected BPMs,
/// then computes the rate that retempos B onto A via the SAME pure helper the
/// engine's `sync_deck` uses (`sync_rate`), and prints the resulting matched
/// BPM. Asserts the match lands on A's tempo (within the rate clamp). No GUI,
/// no audio device — runs anywhere for autonomous verification of the math.
fn selftest_sync(file_a: &str, file_b: &str) -> Result<()> {
    use livehub_engine::{decode_track, sync_rate};

    println!("== livehub sync test ==");
    println!("  A: {file_a}");
    println!("  B: {file_b}");
    let ta = decode_track(std::path::Path::new(file_a), 48_000)?;
    let tb = decode_track(std::path::Path::new(file_b), 48_000)?;
    let (bpm_a, bpm_b) = (ta.bpm, tb.bpm);
    println!("  bpmA = {bpm_a:.2}");
    println!("  bpmB = {bpm_b:.2}");

    // Retempo B onto A: rate makes B's effective bpm == A's bpm.
    let rate = sync_rate(bpm_b, bpm_a);
    let matched = bpm_b * rate as f64;
    println!("  sync_rate(bpmB → bpmA) = {rate:.6}");
    println!("  matched BPM (bpmB × rate) = {matched:.2}");

    // Verdict: with both BPMs detected and the ratio inside the clamp, the
    // matched tempo must equal A's. If clamped (>4× / <0.25×) it can't reach A.
    let ok = if bpm_a > 0.0 && bpm_b > 0.0 {
        (matched - bpm_a).abs() < 0.5
    } else {
        (rate - 1.0).abs() < 1e-6 // un-analyzed → no-op rate 1.0
    };
    println!("  sync {}", if ok { "PASS" } else { "FAIL (clamped or bpm missing)" });
    Ok(())
}

/// Headless CLAP end-to-end test of the master-bus insert. Loads the named
/// SuperDuper effect via the SAME `MasterFx` path the engine uses (48k, 512
/// max frames), feeds one 512-frame block of a nonzero 220 Hz sine into both
/// channels, runs `process()`, and asserts the output is not bit-identical to
/// the input — which proves the audio-ports + processing plumbing is wired. No
/// audio device, no window, so it runs anywhere for autonomous verification.
fn selftest_fx(plugin: &str) -> Result<()> {
    use livehub_engine::MasterFx;

    println!("== livehub fx test: {plugin} ==");
    let needle = plugin.to_lowercase();
    let path = livehub_engine::list_superduper_clap().into_iter().find(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.to_lowercase().contains(&needle))
    });
    let Some(path) = path else {
        println!("  fx FAILED: no installed SuperDuper*.clap matches '{plugin}'");
        return Ok(());
    };
    println!("  plugin : {}", path.display());

    const N: usize = 512;
    let (mut fx, _tx, params) = match MasterFx::load(&path, 48_000, N as u32) {
        Ok(t) => t,
        Err(e) => {
            println!("  fx FAILED: {e:#}");
            return Ok(());
        }
    };
    println!("  name   : {}", fx.plugin_name);
    println!("  params : {}", params.len());

    // A 220 Hz sine at 48 kHz — nonzero, sub-Nyquist test tone.
    let make = || -> Vec<f32> {
        (0..N)
            .map(|i| (std::f32::consts::TAU * 220.0 * i as f32 / 48_000.0).sin() * 0.5)
            .collect()
    };
    let in_l = make();
    let in_r = make();
    let mut l = in_l.clone();
    let mut r = in_r.clone();

    fx.process(&mut l, &mut r);

    let changed = (0..N)
        .filter(|&i| l[i].to_bits() != in_l[i].to_bits() || r[i].to_bits() != in_r[i].to_bits())
        .count();
    if changed > 0 {
        println!("  fx OK: {} changed {changed}/{N} samples", fx.plugin_name);
    } else {
        println!("  fx UNCHANGED (suspicious) — output bit-identical to input");
    }
    Ok(())
}

/// Headless check that the loop sizes (`gui::LOOP_SIZES`) produce correct
/// regions: each is exactly `k` beats long, the start sits on the beat
/// at-or-before the playhead, and halving the size halves the length. Decodes
/// the file for its real beat grid, then exercises the same pure helper the GUI
/// calls — so loop math is verified without a window or audio device.
fn selftest_loops(file: &str) -> Result<()> {
    let path = std::path::Path::new(file);
    println!("== livehub loop test: {file} ==");
    let t = livehub_engine::decode_track(path, 48_000)?;
    println!(
        "  grid : bpm={:.1}, {} beats, {} downbeats, sr={}",
        t.bpm,
        t.beats.len(),
        t.downbeats.len(),
        t.sample_rate
    );
    if t.beats.len() < 2 {
        println!("  SKIP: no beat grid");
        return Ok(());
    }
    let sr = t.sample_rate as f64;
    let nominal_beat = if t.bpm > 0.0 { 60.0 / t.bpm } else { f64::NAN };
    // Engage from a fixed early playhead so every size has room to fit.
    let ph = t.beats[1];
    println!("  playhead = {ph:.3}s");
    println!(
        "  {:>4} {:>5} {:>13} {:>8} {:>8}  result",
        "size", "beats", "frames", "len(s)", "nom(s)"
    );

    let mut prev_len: Option<f64> = None;
    let mut all_ok = true;
    for (label, k) in gui::LOOP_SIZES {
        match livehub_engine::beat_loop_region(&t.beats, ph, k, t.sample_rate) {
            Some((s, e)) => {
                let (start_sec, end_sec) = (s as f64 / sr, e as f64 / sr);
                let len_s = end_sec - start_sec;
                let nom = nominal_beat * k as f64;
                // Independent recount: beats inside [start, end) must be exactly k.
                let span = t
                    .beats
                    .iter()
                    .filter(|&&b| b >= start_sec - 1e-3 && b < end_sec - 1e-3)
                    .count();
                let exact = span == k;
                // Each size (except the first) should be ~2× the previous one.
                let doubles = match prev_len {
                    Some(p) => (len_s - 2.0 * p).abs() <= 0.6 * p,
                    None => true,
                };
                let ok = exact && doubles && e > s;
                all_ok &= ok;
                let note = if ok {
                    "PASS".to_string()
                } else {
                    format!("FAIL (span={span}, doubles={doubles})")
                };
                println!(
                    "  {label:>4} {k:>5} {:>13} {len_s:>8.3} {nom:>8.3}  {note}",
                    format!("{s}..{e}")
                );
                prev_len = Some(len_s);
            }
            None => {
                println!(
                    "  {label:>4} {k:>5} {:>13} {:>8} {:>8}  no-op (not enough beats left)",
                    "—", "—", "—"
                );
            }
        }
    }
    println!("  overall: {}", if all_ok { "PASS" } else { "FAIL" });
    Ok(())
}

/// Headless pipeline check: decode + beat-analyze the audio and probe the video,
/// printing what would drive the deck. No audio device, no window — so it runs
/// anywhere and its output can be read back for autonomous verification.
fn selftest(file: &str) -> Result<()> {
    let path = std::path::Path::new(file);
    println!("== livehub selftest: {file} ==");
    match livehub_engine::decode_track(path, 48_000) {
        Ok(t) => {
            println!(
                "  audio : {:.1}s, {} frames @ {} Hz",
                t.duration_secs(),
                t.frames,
                t.sample_rate
            );
            println!(
                "  beats : bpm={:.1}, {} beats, {} downbeats",
                t.bpm,
                t.beats.len(),
                t.downbeats.len()
            );
            println!("  wave  : {} peak bins", t.waveform.len());
        }
        Err(e) => println!("  audio : DECODE FAILED: {e:#}"),
    }
    println!("{}", video::selftest_video(path));
    Ok(())
}

/// Load a track and play it mixed with the live mic until it ends (or forever
/// with `--looped`), printing the playhead. The egui transport replaces this.
fn play(device: &str, file: &str, looped: bool) -> Result<()> {
    use std::io::Write;
    use std::time::Duration;

    let eng = livehub_engine::Engine::new(device, livehub_core::OutputRouting::default(), None)?;
    eprintln!("decoding '{file}' … (mic is already live)");
    let secs = eng.load_track(0, std::path::Path::new(file))?;
    eprintln!("loaded ({secs:.1}s) — deck + live mic; Ctrl-C to stop");
    eng.play(0);
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let ph = eng.playhead_secs(0);
        eprint!("\r{ph:6.1} / {secs:6.1} s   ");
        let _ = std::io::stderr().flush();
        if ph >= secs - 0.05 {
            if looped {
                eng.seek_secs(0, 0.0);
            } else {
                break;
            }
        }
    }
    eprintln!("\ndone (underruns: {})", eng.underruns());
    Ok(())
}
