//! livehub CLI — for M0 this is a headless audio test harness (list devices,
//! mic→master passthrough, channel sweep). The egui surface arrives in M1.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod gui;
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
    }
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
        "  {:>4} {:>5} {:>13} {:>8} {:>8}  {}",
        "size", "beats", "frames", "len(s)", "nom(s)", "result"
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
