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
mod last_phone;
mod link;
mod loops;
mod mic_monitor;
mod remote;
mod track;
mod wav;

pub use audio::{run_passthrough, run_sweep};
pub use broadcast::{run_broadcast, run_broadcast_test, Broadcaster, LevelMonitor};
pub use device::input_device_names;
pub use mic_monitor::MicMonitor;
pub use discovery::{is_self_addr, Discovery, Peer};
pub use last_phone::{load_last_phone, save_last_phone};
pub use remote::{
    camparam_packet, control_packet, send_control, PreviewReceiver, RemoteControl, CTRL_MAGIC,
    PREVIEW_MAGIC,
    CAM_AE_LOCK, CAM_EV, CAM_FLIP, CAM_ISO, CAM_LENS,
    CAM_LENS_SELECT, CAM_MANUAL_EXP, CAM_MANUAL_FOCUS, CAM_SHUTTER, CMD_HEARTBEAT,
    CMD_PREVIEW_QUALITY, CMD_PREVIEW_START, CMD_PREVIEW_STOP, CMD_REC_START, CMD_REC_STOP,
    CMD_REC_TOGGLE,
    GEST_BPM, GEST_BURN_HUD, GEST_CYCLE_MODE, GEST_MIDI, GEST_MODE, GEST_NOTELEN, GEST_NOTEMODE,
    GEST_PROFILE, GEST_TRACKING,
};
pub use clap_fx::{list_superduper_clap, MasterFx, ParamInfo, ParamSet};
pub use decoder::decode_track;
pub use device::list_devices;
pub use engine::{sync_rate, Engine, Transport};
pub use link::LinkHub;
pub use loops::beat_loop_region;
pub use track::TrackBuf;

// Exercise the reuse path at build time; used for real in M2.5 (beatgrid).
pub use video_audio::beats::detect_beats;
