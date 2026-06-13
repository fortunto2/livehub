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
    /// Auto-load this media file into the deck on GUI launch.
    #[arg(long)]
    open: Option<std::path::PathBuf>,
    /// Start playing immediately after the auto-loaded file finishes decoding.
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        None => gui::run(&cli.device, parse_aux(cli.aux.as_deref()), cli.open, cli.play),
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
    }
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
    let secs = eng.load_track(std::path::Path::new(file))?;
    eprintln!("loaded ({secs:.1}s) — deck + live mic; Ctrl-C to stop");
    eng.play();
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let ph = eng.playhead_secs();
        eprint!("\r{ph:6.1} / {secs:6.1} s   ");
        let _ = std::io::stderr().flush();
        if ph >= secs - 0.05 {
            if looped {
                eng.seek_secs(0.0);
            } else {
                break;
            }
        }
    }
    eprintln!("\ndone (underruns: {})", eng.underruns());
    Ok(())
}
