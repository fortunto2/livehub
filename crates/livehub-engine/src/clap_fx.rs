//! Master-bus CLAP insert — hosts ONE SuperDuper effect on the master mix.
//!
//! The design is the lowest-risk one from the M2 recon: the started CLAP audio
//! processor is NOT freely `Send`, so we never move a live `StartedPluginAudio-
//! Processor` across threads. Instead a whole [`MasterFx`] (instance → activate
//! → start_processing → all scratch buffers) is built on the main thread inside
//! [`MasterFx::load`] and then moved ONCE into the cpal output callback that
//! owns it — exactly like `sdsp-runner` does. To load/unload an effect at run
//! time the engine rebuilds the output stream (see `engine.rs`), so the
//! processor is created on, and only ever used on, the audio thread that runs
//! `process()`.
//!
//! Param changes flow lock-free WITHOUT a rebuild: the GUI pushes [`ParamSet`]
//! onto a wait-free `rtrb` ring whose consumer lives inside `MasterFx`; the
//! audio callback drains all pending sets into `ParamValueEvent`s at the top of
//! each `process()`. No heap alloc / lock / syscall happens in `process()`:
//! every buffer is pre-allocated in `load()`.
//!
//! The Host impl (`Host` / `HostShared` / `host_info`) is cribbed verbatim from
//! `superduper-dsp/tools/sdsp-runner/src/host.rs` — minimum viable host that
//! only registers the `log` extension.

use anyhow::{Context, Result};
use clack_common::events::Pckn;
use clack_common::events::event_types::ParamValueEvent;
use clack_common::utils::{ClapId, Cookie};
use clack_extensions::log::{HostLog, HostLogImpl, LogSeverity};
use clack_extensions::params::{ParamInfoBuffer, PluginParams};
use clack_host::events::io::{EventBuffer, InputEvents, OutputEvents};
use clack_host::prelude::*;
use clack_host::process::audio_buffers::{
    AudioPortBuffer, AudioPortBufferType, AudioPorts, InputChannel,
};
use std::path::Path;

// ---------------------------------------------------------------------------
// Host — minimum viable: forward plugin log lines to stderr, stub the rest.
// Cribbed verbatim from sdsp-runner/src/host.rs.
// ---------------------------------------------------------------------------

pub struct Host;
pub struct HostShared;

impl SharedHandler<'_> for HostShared {
    fn request_restart(&self) {
        eprintln!("[clap_fx] request_restart");
    }
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

impl HostLogImpl for HostShared {
    fn log(&self, severity: LogSeverity, message: &str) {
        eprintln!("[plugin {severity}] {message}");
    }
}

impl HostHandlers for Host {
    type Shared<'a> = HostShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();

    fn declare_extensions(builder: &mut HostExtensions<Self>, _shared: &Self::Shared<'_>) {
        builder.register::<HostLog>();
    }
}

fn host_info() -> Result<HostInfo> {
    HostInfo::new(
        "livehub",
        "SuperDuperAI",
        "https://github.com/fortunto2/superduper-dsp",
        "0.1.0",
    )
    .context("HostInfo::new failed")
}

// ---------------------------------------------------------------------------
// Public param model
// ---------------------------------------------------------------------------

/// One parameter's static metadata, enumerated once at load and handed to the
/// GUI to render a slider (`min..=max`, starting at `value`).
#[derive(Clone, Debug)]
pub struct ParamInfo {
    pub id: u32,
    pub name: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub value: f64,
}

/// A single param change request, pushed by the GUI onto a wait-free ring and
/// drained by the audio thread into a `ParamValueEvent`. `value` is in the
/// param's plain (un-normalized) range.
#[derive(Clone, Copy, Debug)]
pub struct ParamSet {
    pub id: u32,
    pub value: f64,
}

// ---------------------------------------------------------------------------
// MasterFx — the master-bus insert. Built on the main thread, then moved into
// the output callback that owns it.
// ---------------------------------------------------------------------------

/// A live master-bus CLAP insert: an instantiated + activated + started plugin
/// plus all the pre-allocated scratch the audio thread needs to run it in
/// place on the master stereo mix. Construct via [`MasterFx::load`]; call
/// [`MasterFx::process`] once per cpal callback from the audio thread.
pub struct MasterFx {
    /// The started audio processor. This is the ONLY long-lived CLAP handle we
    /// keep: it is `Send` (so the cpal callback can own it), and it holds an
    /// `Arc<PluginInstanceInner>` that keeps the plugin (and its dylib/entry)
    /// alive. The `PluginInstance` itself is dropped at the end of `load()` —
    /// `PluginInstance::drop` deliberately leaks its Arc when an audio processor
    /// is still around, and `PluginInstanceInner::drop` runs (deactivate +
    /// destroy) when `started` finally drops. This is exactly the pattern
    /// sdsp-runner relies on (`PluginInstance` is `!Send`, the processor is).
    started: StartedPluginAudioProcessor<Host>,

    /// Param-change consumer (the GUI keeps the producer). Drained each block.
    param_rx: rtrb::Consumer<ParamSet>,

    /// Pre-allocated stereo scratch sized to `max_frames`.
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    out_l: Vec<f32>,
    out_r: Vec<f32>,
    max_frames: usize,

    /// Port objects allocated once; their per-block borrows happen in process().
    input_ports: AudioPorts,
    output_ports: AudioPorts,

    /// Event buffers reused every block (cleared + refilled, never reallocated).
    in_events: EventBuffer,
    out_events: EventBuffer,

    pub plugin_name: String,
}

impl MasterFx {
    /// Load a `.clap` bundle, instantiate + activate + start it at `sample_rate`
    /// with a max block of `max_frames`, pre-allocate all scratch, and enumerate
    /// its params.
    ///
    /// Returns the `MasterFx` (moved into the audio callback), the `rtrb`
    /// producer (the GUI/engine keeps it to push [`ParamSet`]s), and the param
    /// list (for the GUI to build sliders).
    pub fn load(
        path: &Path,
        sample_rate: u32,
        max_frames: u32,
    ) -> Result<(MasterFx, rtrb::Producer<ParamSet>, Vec<ParamInfo>)> {
        // Load the bundle + read its descriptor id (CLAP bundles are dirs on
        // macOS; PluginEntry::load handles both bundle-dir and inner dylib).
        // SAFETY: we trust the installed plugin, same as REAPER / Bitwig.
        let entry = unsafe { PluginEntry::load(path) }
            .with_context(|| format!("PluginEntry::load failed for {}", path.display()))?;
        let factory = entry
            .get_factory::<clack_host::factory::plugin::PluginFactory>()
            .context("plugin exposes no PluginFactory")?;
        if factory.plugin_count() == 0 {
            anyhow::bail!("plugin factory is empty");
        }
        let desc = factory
            .plugin_descriptor(0)
            .context("could not read first plugin descriptor")?;
        let id = desc
            .id()
            .context("plugin has no id")?
            .to_str()
            .context("plugin id is not UTF-8")?
            .to_string();
        let plugin_name = desc
            .name()
            .and_then(|n| n.to_str().ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| id.clone());
        let id_cstr = std::ffi::CString::new(id).context("plugin id has interior NUL")?;

        let host_info = host_info()?;
        let mut instance = PluginInstance::<Host>::new(
            |_| HostShared,
            |_| (),
            &entry,
            &id_cstr,
            &host_info,
        )
        .context("plugin instantiation failed")?;

        // Enumerate params via the plugin-side params extension (read on the
        // main thread, before activation). Empty list if the plugin has none.
        let params = enumerate_params(&mut instance);

        let cfg = PluginAudioConfiguration {
            sample_rate: sample_rate as f64,
            // The cpal callback hands variable-size buffers; we sub-block to
            // `max_frames` so 1..=max is always valid.
            min_frames_count: 1,
            max_frames_count: max_frames,
        };
        let stopped = instance
            .activate(|_, _| (), cfg)
            .context("plugin activate() failed")?;
        let started = stopped
            .start_processing()
            .map_err(|_| anyhow::anyhow!("plugin start_processing() failed"))?;

        let (producer, consumer) = rtrb::RingBuffer::<ParamSet>::new(256);

        // Drop the instance: it leaks its Arc (because `started` holds one), so
        // the plugin stays alive; the real teardown happens when `started` drops.
        drop(instance);

        let block = max_frames as usize;
        let fx = MasterFx {
            started,
            param_rx: consumer,
            in_l: vec![0.0; block],
            in_r: vec![0.0; block],
            out_l: vec![0.0; block],
            out_r: vec![0.0; block],
            max_frames: block,
            input_ports: AudioPorts::with_capacity(2, 1),
            output_ports: AudioPorts::with_capacity(2, 1),
            in_events: EventBuffer::with_capacity(256),
            out_events: EventBuffer::with_capacity(64),
            plugin_name,
        };
        Ok((fx, producer, params))
    }

    /// Process the master stereo mix in place: drain all pending [`ParamSet`]s
    /// into param events, copy `left`/`right` into the plugin input, run the
    /// plugin, and copy its output back over `left`/`right`. RT-safe — no
    /// allocation, no lock, no syscall.
    ///
    /// `left` and `right` must be the same length and `<= max_frames`; the
    /// engine sub-blocks the cpal buffer to guarantee this. On any plugin error
    /// the audio is left unchanged (passes through dry).
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len().min(right.len()).min(self.max_frames);
        if n == 0 {
            return;
        }

        // Drain ALL pending param changes into input ParamValueEvents.
        self.in_events.clear();
        while let Ok(ps) = self.param_rx.pop() {
            if let Some(id) = ClapId::from_raw(ps.id) {
                let ev = ParamValueEvent::new(
                    0,
                    id,
                    Pckn::new(0u16, 0u16, 0u16, 0u32),
                    ps.value,
                    Cookie::empty(),
                );
                self.in_events.push(&ev);
            }
        }

        // Copy the master mix into the plugin input scratch.
        self.in_l[..n].copy_from_slice(&left[..n]);
        self.in_r[..n].copy_from_slice(&right[..n]);

        let in_chans: [&mut [f32]; 2] = [&mut self.in_l[..n], &mut self.in_r[..n]];
        let input_audio = self.input_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                in_chans.into_iter().map(InputChannel::variable),
            ),
        }]);
        let mut out_chans: [&mut [f32]; 2] = [&mut self.out_l[..n], &mut self.out_r[..n]];
        let mut output_audio = self.output_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(
                out_chans.iter_mut().map(|b| &mut **b),
            ),
        }]);

        let input_events = InputEvents::from_buffer(&self.in_events);
        self.out_events.clear();
        let mut output_events = OutputEvents::from_buffer(&mut self.out_events);

        let ok = self
            .started
            .process(
                &input_audio,
                &mut output_audio,
                &input_events,
                &mut output_events,
                None,
                None,
            )
            .is_ok();

        if ok {
            left[..n].copy_from_slice(&self.out_l[..n]);
            right[..n].copy_from_slice(&self.out_r[..n]);
        }
        // On error, leave `left`/`right` untouched → dry passthrough.
    }
}

/// Enumerate the plugin's params via the plugin-side `params` extension. Reads
/// id / name / min / max / default / current value for each. Returns `[]` if
/// the plugin doesn't expose the extension.
fn enumerate_params(instance: &mut PluginInstance<Host>) -> Vec<ParamInfo> {
    let mut handle = instance.plugin_handle();
    let Some(ext) = handle.shared().get_extension::<PluginParams>() else {
        return Vec::new();
    };
    let count = ext.count(&mut handle);
    let mut out = Vec::with_capacity(count as usize);
    let mut buf = ParamInfoBuffer::new();
    for idx in 0..count {
        let Some(info) = ext.get_info(&mut handle, idx, &mut buf) else {
            continue;
        };
        let id = info.id;
        let value = ext.get_value(&mut handle, id).unwrap_or(info.default_value);
        out.push(ParamInfo {
            id: id.get(),
            name: String::from_utf8_lossy(info.name).into_owned(),
            min: info.min_value,
            max: info.max_value,
            default: info.default_value,
            value,
        });
    }
    out
}

/// Scan `~/Library/Audio/Plug-Ins/CLAP/` for installed `SuperDuper*.clap`
/// bundles, returned sorted by file name. Effects and instruments both match;
/// the user picks. Returns `[]` if the directory doesn't exist.
pub fn list_superduper_clap() -> Vec<std::path::PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let dir = Path::new(&home).join("Library/Audio/Plug-Ins/CLAP");
    let mut out: Vec<std::path::PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("SuperDuper") && n.ends_with(".clap"))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}
