//! livehub-core — pure entities for the live DJ/VJ hub.
//!
//! No realtime deps (no cpal/egui). Mirrors the DDD layering of the
//! `video-generator-agent` crates this app reuses: entities here, services and
//! the realtime runtime in `livehub-engine`, UI in `app`.

// Re-export the reused beat model so the whole app speaks one vocabulary — and
// so the cross-workspace path-dep on `video-core` is exercised at build time.
pub use video_core::beats::{BeatConfig, BeatResult};

/// Identifies a deck (audio now; video later in M3+).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeckId(pub u8);

/// A bus a gain / insert applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BusId {
    Master,
    Mic,
    Deck(DeckId),
}

/// Which interleaved channel indices the master (and optional cue) feed on the
/// chosen output device.
///
/// On a CoreAudio aggregate ("Mixx Rig") the channel order is empirical — there
/// is no API to query "which index is the Arrow line-out". Keep it a runtime
/// config, resolved by the M0 tone-sweep. Defaults assume the Arrow is first in
/// the aggregate: MON L/R = 0/1, HP L/R = 4/5.
#[derive(Debug, Clone, Copy)]
pub struct OutputRouting {
    pub master_l: usize,
    pub master_r: usize,
    pub cue_l: Option<usize>,
    pub cue_r: Option<usize>,
}

impl Default for OutputRouting {
    fn default() -> Self {
        Self { master_l: 0, master_r: 1, cue_l: Some(4), cue_r: Some(5) }
    }
}
