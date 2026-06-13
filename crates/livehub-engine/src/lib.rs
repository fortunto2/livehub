//! livehub-engine — the realtime runtime (cpal, symphonia; clack-host / rubato
//! later) plus device management.
//!
//! The lock-free audio callbacks live in this crate and nowhere else. They obey
//! the same `process()`-rules as the SuperDuper plugins: no heap alloc, no
//! locks, no syscalls inside a callback.

mod audio;
mod decoder;
mod device;
mod engine;
mod link;
mod loops;
mod track;

pub use audio::{run_passthrough, run_sweep};
pub use decoder::decode_track;
pub use device::list_devices;
pub use engine::{Engine, Transport};
pub use link::LinkHub;
pub use loops::beat_loop_region;
pub use track::TrackBuf;

// Exercise the reuse path at build time; used for real in M2.5 (beatgrid).
pub use video_audio::beats::detect_beats;
