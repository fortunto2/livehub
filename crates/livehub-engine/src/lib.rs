//! livehub-engine — the realtime runtime (cpal, symphonia; clack-host / rubato
//! later) plus device management.
//!
//! RT-callback rules (CANONICAL — the lock-free audio callbacks live in this
//! crate and nowhere else; they obey the same `process()`-rules as the
//! SuperDuper plugins): no heap alloc, no locks, no syscalls inside a callback.
//! Pattern: snapshot the shared atomics ONCE per block into stack locals, then
//! work from those — never re-load mid-block.

mod audio;
mod broadcast;
mod clap_fx;
mod decoder;
mod device;
mod discovery;
mod engine;
mod link;
mod loops;
mod track;

pub use audio::{run_passthrough, run_sweep};
pub use broadcast::{run_broadcast, Broadcaster};
pub use device::input_device_names;
pub use discovery::{Discovery, Peer};
pub use clap_fx::{list_superduper_clap, MasterFx, ParamInfo, ParamSet};
pub use decoder::decode_track;
pub use device::list_devices;
pub use engine::{sync_rate, Engine, Transport};
pub use link::LinkHub;
pub use loops::beat_loop_region;
pub use track::TrackBuf;

// Exercise the reuse path at build time; used for real in M2.5 (beatgrid).
pub use video_audio::beats::detect_beats;
