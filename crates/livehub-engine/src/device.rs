//! cpal device discovery + name-based picking + full-channel config selection.
//!
//! We never trust `default_*_device()` for the live rig (the output is a named
//! aggregate / the Arrow, rarely the system default), and we never trust
//! `default_*_config()` for the channel count — on a CoreAudio aggregate that
//! reports a 2-channel *default* even though the device exposes 8/12 channels.
//! `best_*_config` instead picks the F32 config with the MOST channels so we can
//! address the Arrow monitors, headphones (cue), and BlackHole all at once.

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait};

/// Print every input + output device with default vs max channel count, default
/// rate and format. The M0 `--list-devices` acceptance helper.
/// Input device names (for the GUI broadcast source picker). Empty on failure.
pub fn input_device_names() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(devs) => devs.filter_map(|d| d.name().ok()).collect(),
        Err(_) => Vec::new(),
    }
}

pub fn list_devices() -> Result<()> {
    let host = cpal::default_host();
    println!("== OUTPUT devices ==");
    for dev in host.output_devices().context("enumerating output devices")? {
        print_dev(&dev, false);
    }
    println!("== INPUT devices ==");
    for dev in host.input_devices().context("enumerating input devices")? {
        print_dev(&dev, true);
    }
    Ok(())
}

fn print_dev(dev: &cpal::Device, input: bool) {
    let name = dev.name().unwrap_or_else(|_| "<unknown>".into());
    let default = if input {
        dev.default_input_config().ok()
    } else {
        dev.default_output_config().ok()
    };
    let max_ch = max_channels(dev, input);
    match default {
        Some(cfg) => println!(
            "  {name:32}  default {:>2}ch / max {:>2}ch  {:>6} Hz  {:?}",
            cfg.channels(),
            max_ch,
            cfg.sample_rate().0,
            cfg.sample_format()
        ),
        None => println!("  {name:32}  default <none> / max {max_ch}ch"),
    }
}

fn max_channels(dev: &cpal::Device, input: bool) -> u16 {
    let configs = if input {
        dev.supported_input_configs().ok().map(itertools_max)
    } else {
        dev.supported_output_configs().ok().map(itertools_max)
    };
    configs.unwrap_or(0)
}

fn itertools_max<I: Iterator<Item = cpal::SupportedStreamConfigRange>>(it: I) -> u16 {
    it.map(|r| r.channels()).max().unwrap_or(0)
}

/// Output device whose name contains `hint` (case-insensitive); falls back to
/// the system default output.
pub(crate) fn pick_output(hint: &str) -> Result<cpal::Device> {
    let host = cpal::default_host();
    let hint_lc = hint.to_lowercase();
    if let Some(d) = host.output_devices()?.find(|d| name_matches(d, &hint_lc)) {
        return Ok(d);
    }
    host.default_output_device()
        .context("no matching output device and no system default")
}

/// Input device whose name contains `hint` (case-insensitive); falls back to
/// the system default input.
pub(crate) fn pick_input(hint: &str) -> Result<cpal::Device> {
    let host = cpal::default_host();
    let hint_lc = hint.to_lowercase();
    if let Some(d) = host.input_devices()?.find(|d| name_matches(d, &hint_lc)) {
        return Ok(d);
    }
    host.default_input_device()
        .context("no matching input device and no system default")
}

fn name_matches(d: &cpal::Device, hint_lc: &str) -> bool {
    d.name()
        .map(|n| n.to_lowercase().contains(hint_lc))
        .unwrap_or(false)
}

/// The F32 output config with the most channels, at 48 kHz when available.
pub(crate) fn best_output_config(dev: &cpal::Device) -> Result<cpal::SupportedStreamConfig> {
    best_config(
        dev.supported_output_configs()
            .context("supported_output_configs")?,
        PREFERRED_SR,
    )
}

/// The F32 input config with the most channels, at 48 kHz when available.
pub(crate) fn best_input_config(dev: &cpal::Device) -> Result<cpal::SupportedStreamConfig> {
    best_input_config_at(dev, PREFERRED_SR)
}

/// Like [`best_input_config`] but targeting a specific sample rate (falls back to
/// the device's max if it can't run at `target_sr`).
pub(crate) fn best_input_config_at(
    dev: &cpal::Device,
    target_sr: u32,
) -> Result<cpal::SupportedStreamConfig> {
    best_config(
        dev.supported_input_configs().context("supported_input_configs")?,
        target_sr,
    )
}

/// Preferred device rate — the whole rig (Arrow, video, monitoring) runs at 48k.
const PREFERRED_SR: u32 = 48_000;

fn best_config<I>(ranges: I, target_sr: u32) -> Result<cpal::SupportedStreamConfig>
where
    I: Iterator<Item = cpal::SupportedStreamConfigRange>,
{
    let target = cpal::SampleRate(target_sr);
    let f32: Vec<cpal::SupportedStreamConfigRange> = ranges
        .filter(|r| r.sample_format() == cpal::SampleFormat::F32)
        .collect();
    let supports = |r: &cpal::SupportedStreamConfigRange| {
        r.min_sample_rate() <= target && target <= r.max_sample_rate()
    };

    // Prefer a config that can run at 48 kHz, choosing the one with the most
    // channels. Only if no config offers 48 kHz do we fall back to the
    // most-channels config at its own max rate (e.g. the Arrow stuck at 44.1).
    if let Some(r) = f32.iter().filter(|r| supports(r)).max_by_key(|r| r.channels()) {
        return Ok((*r).with_sample_rate(target));
    }
    let r = f32
        .iter()
        .max_by_key(|r| r.channels())
        .ok_or_else(|| anyhow!("device exposes no F32 stream config"))?;
    Ok((*r).with_max_sample_rate())
}
