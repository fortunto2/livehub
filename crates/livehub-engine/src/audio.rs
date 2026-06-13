//! M0 realtime audio: mic → master passthrough and a channel-identification
//! tone sweep. Both run lock-free callbacks (no alloc, no syscalls inside the
//! cpal data closures).

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::device::{best_input_config, best_output_config, pick_input, pick_output};

/// Mic → master passthrough on a single (aggregate) device. The input and
/// output streams share the device clock, so the SPSC ring between them stays
/// bounded with no drift. Blocks until Ctrl-C.
pub fn run_passthrough(
    device_hint: &str,
    mic_ch: usize,
    master_l: usize,
    master_r: usize,
) -> Result<()> {
    let out_dev = pick_output(device_hint)?;
    let in_dev = pick_input(device_hint)?;
    eprintln!("output = '{}'", out_dev.name().unwrap_or_default());
    eprintln!("input  = '{}'", in_dev.name().unwrap_or_default());

    let out_supported = best_output_config(&out_dev)?;
    let in_supported = best_input_config(&in_dev)?;

    if out_supported.sample_format() != cpal::SampleFormat::F32
        || in_supported.sample_format() != cpal::SampleFormat::F32
    {
        return Err(anyhow!(
            "M0 requires F32 streams (out={:?}, in={:?})",
            out_supported.sample_format(),
            in_supported.sample_format()
        ));
    }

    let out_ch = out_supported.channels() as usize;
    let in_ch = in_supported.channels() as usize;
    eprintln!(
        "out: {out_ch} ch @ {} Hz | in: {in_ch} ch @ {} Hz",
        out_supported.sample_rate().0,
        in_supported.sample_rate().0
    );
    eprintln!("routing: mic in-ch {mic_ch} -> master out-ch {master_l}/{master_r}");

    if master_l >= out_ch || master_r >= out_ch {
        return Err(anyhow!("master channel out of range (device has {out_ch} out channels)"));
    }
    if mic_ch >= in_ch {
        return Err(anyhow!("mic channel out of range (device has {in_ch} in channels)"));
    }

    // SPSC ring: input callback (producer) -> output callback (consumer).
    // Interleaved stereo f32, ~1 s of slack at the device rate.
    let cap = out_supported.sample_rate().0.max(in_supported.sample_rate().0) as usize * 2;
    let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(cap);

    let underruns = Arc::new(AtomicU64::new(0));
    let underruns_cb = underruns.clone();

    let in_cfg: cpal::StreamConfig = in_supported.into();
    let in_stream = in_dev
        .build_input_stream(
            &in_cfg,
            move |inp: &[f32], _: &cpal::InputCallbackInfo| {
                let frames = inp.len() / in_ch;
                for f in 0..frames {
                    let s = inp.get(f * in_ch + mic_ch).copied().unwrap_or(0.0);
                    // Mono mic -> stereo pair. Drop on overrun (matched clock).
                    let _ = prod.push(s);
                    let _ = prod.push(s);
                }
            },
            |e| eprintln!("input stream error: {e}"),
            None,
        )
        .context("build_input_stream")?;

    let out_cfg: cpal::StreamConfig = out_supported.into();
    let out_stream = out_dev
        .build_output_stream(
            &out_cfg,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let frames = out.len() / out_ch;
                for f in 0..frames {
                    let base = f * out_ch;
                    for c in 0..out_ch {
                        out[base + c] = 0.0;
                    }
                    let l = cons.pop().unwrap_or_else(|_| {
                        underruns_cb.fetch_add(1, Ordering::Relaxed);
                        0.0
                    });
                    let r = cons.pop().unwrap_or(0.0);
                    out[base + master_l] = l;
                    out[base + master_r] = r;
                }
            },
            |e| eprintln!("output stream error: {e}"),
            None,
        )
        .context("build_output_stream")?;

    in_stream.play().context("input stream play")?;
    out_stream.play().context("output stream play")?;
    eprintln!("passthrough running — clap into the mic; Ctrl-C to stop");

    loop {
        std::thread::sleep(std::time::Duration::from_secs(3));
        let u = underruns.load(Ordering::Relaxed);
        if u > 0 {
            eprintln!("note: {u} output underruns so far (ring starved)");
        }
    }
}

/// Play a 440 Hz tone on ONE output channel at a time, cycling every ~1.5 s, so
/// you can hear which interleaved index reaches the Arrow monitors (= master_l).
pub fn run_sweep(device_hint: &str) -> Result<()> {
    let dev = pick_output(device_hint)?;
    eprintln!("output = '{}'", dev.name().unwrap_or_default());
    let supported = best_output_config(&dev)?;
    let out_ch = supported.channels() as usize;
    let sr = supported.sample_rate().0 as f32;
    eprintln!("sweeping 440 Hz across {out_ch} channels, ~1.5 s each — note which one hits the monitors");

    let active = Arc::new(AtomicU64::new(u64::MAX)); // no channel until set
    let active_cb = active.clone();
    let mut phase = 0.0_f32;
    let dt = 1.0 / sr;

    let cfg: cpal::StreamConfig = supported.into();
    let stream = dev
        .build_output_stream(
            &cfg,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let ch = active_cb.load(Ordering::Relaxed) as usize;
                let frames = out.len() / out_ch;
                for f in 0..frames {
                    let base = f * out_ch;
                    let s = (phase * std::f32::consts::TAU).sin() * 0.2;
                    phase += 440.0 * dt;
                    if phase >= 1.0 {
                        phase -= 1.0;
                    }
                    for c in 0..out_ch {
                        out[base + c] = if c == ch { s } else { 0.0 };
                    }
                }
            },
            |e| eprintln!("output stream error: {e}"),
            None,
        )
        .context("build_output_stream")?;
    stream.play().context("stream play")?;
    eprintln!("looping the sweep — put on headphones / check monitors; Ctrl-C to stop");

    let mut pass = 1u32;
    loop {
        println!("-- pass {pass} --");
        for ch in 0..out_ch {
            active.store(ch as u64, Ordering::Relaxed);
            println!(">> channel index {ch}  (0/1=MON  2/3=VIRTUAL  4/5=HP)");
            std::thread::sleep(std::time::Duration::from_millis(2000));
        }
        pass += 1;
    }
}
