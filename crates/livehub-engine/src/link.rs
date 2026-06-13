//! Ableton Link participation — makes livehub the tempo brain of the rig.
//!
//! Link runs its own networking threads internally; we just create the session,
//! enable it, and capture/commit the shared session state on demand (cheap, fine
//! to call once per UI frame). Any Link-capable app on the LAN — Ableton Live,
//! VirtualDJ, Traktor, or REAPER via the ReaBlink extension — shares one tempo
//! and beat phase with us.

use rusty_link::{AblLink, SessionState};

pub struct LinkHub {
    link: AblLink,
    quantum: f64,
}

impl LinkHub {
    /// Join the Link session at `bpm`. Tempo is shared immediately; transport
    /// (start/stop) sync is OFF by default so livehub drives only the tempo and
    /// doesn't hijack other apps' playback — toggle it on if you want shared play.
    pub fn new(bpm: f64) -> Self {
        let link = AblLink::new(bpm);
        link.enable(true);
        link.enable_start_stop_sync(false);
        Self { link, quantum: 4.0 }
    }

    pub fn start_stop_sync(&self) -> bool {
        self.link.is_start_stop_sync_enabled()
    }

    pub fn set_start_stop_sync(&self, on: bool) {
        self.link.enable_start_stop_sync(on);
    }

    /// Bar length in beats (4 = one 4/4 bar).
    pub fn quantum(&self) -> f64 {
        self.quantum
    }

    pub fn num_peers(&self) -> u64 {
        self.link.num_peers()
    }

    pub fn tempo(&self) -> f64 {
        self.with_state(|s, _| s.tempo())
    }

    pub fn set_tempo(&mut self, bpm: f64) {
        let now = self.link.clock_micros();
        self.mutate_state(|s| s.set_tempo(bpm, now));
    }

    pub fn is_playing(&self) -> bool {
        self.with_state(|s, _| s.is_playing())
    }

    pub fn set_playing(&mut self, playing: bool) {
        let now = self.link.clock_micros();
        self.mutate_state(|s| s.set_is_playing(playing, now));
    }

    /// Phase within the current bar, in beats: `0.0 .. quantum`.
    pub fn beat_phase(&self) -> f64 {
        let now = self.link.clock_micros();
        let q = self.quantum;
        self.with_state(|s, _| s.phase_at_time(now, q))
    }

    fn with_state<R>(&self, f: impl FnOnce(&SessionState, &AblLink) -> R) -> R {
        let mut state = SessionState::new();
        self.link.capture_app_session_state(&mut state);
        f(&state, &self.link)
    }

    fn mutate_state(&self, f: impl FnOnce(&mut SessionState)) {
        let mut state = SessionState::new();
        self.link.capture_app_session_state(&mut state);
        f(&mut state);
        self.link.commit_app_session_state(&state);
    }
}
