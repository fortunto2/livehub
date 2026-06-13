//! Pure beat-loop geometry — shared by the GUI (engaging a loop) and the
//! headless `selftest-loops` harness, so the loop-length math is verified
//! autonomously without a window or an audio device.

/// Frame region `[start, end)` for a `k_beats`-beat loop whose start is the beat
/// at-or-before `playhead_secs`, on the beat grid `beats` (onset times in
/// seconds, ascending). Returns `None` when fewer than `k_beats` beats remain
/// ahead (so we never engage a silently-shortened loop) or the grid is too
/// small. Frames are at `sample_rate` (the deck buffer's rate).
pub fn beat_loop_region(
    beats: &[f64],
    playhead_secs: f64,
    k_beats: usize,
    sample_rate: u32,
) -> Option<(usize, usize)> {
    if beats.len() < 2 || k_beats == 0 {
        return None;
    }
    // First beat at-or-before the playhead (clamped to a valid index).
    let i = beats.partition_point(|&b| b <= playhead_secs).saturating_sub(1);
    let end = i + k_beats;
    if end >= beats.len() {
        return None; // fewer than k beats left → don't engage a short loop.
    }
    let sr = sample_rate as f64;
    Some(((beats[i] * sr) as usize, (beats[end] * sr) as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A perfectly even 120 BPM grid: a beat every 0.5 s.
    fn grid() -> Vec<f64> {
        (0..32).map(|n| n as f64 * 0.5).collect()
    }

    #[test]
    fn region_spans_exactly_k_beats() {
        let g = grid();
        // 4-beat loop from t=0 → frames [0, 4*0.5*48000) = [0, 96000).
        let (s, e) = beat_loop_region(&g, 0.0, 4, 48_000).unwrap();
        assert_eq!(s, 0);
        assert_eq!(e, 96_000);
        // Length in seconds is exactly 4 beats.
        assert!(((e - s) as f64 / 48_000.0 - 2.0).abs() < 1e-6);
    }

    #[test]
    fn halving_k_halves_the_length() {
        let g = grid();
        let len = |k| {
            let (s, e) = beat_loop_region(&g, 2.3, k, 48_000).unwrap();
            e - s
        };
        assert_eq!(len(4), 2 * len(2));
        assert_eq!(len(2), 2 * len(1));
    }

    #[test]
    fn starts_on_beat_at_or_before_playhead() {
        let g = grid();
        // playhead 2.3 s sits between beat 4 (2.0) and 5 (2.5) → start = 2.0 s.
        let (s, _) = beat_loop_region(&g, 2.3, 1, 48_000).unwrap();
        assert_eq!(s, (2.0 * 48_000.0) as usize);
    }

    #[test]
    fn none_when_not_enough_beats_left() {
        let g = grid(); // 32 beats, last index 31.
        // From near the end, a 16-beat loop can't fit.
        assert!(beat_loop_region(&g, 15.9, 16, 48_000).is_none());
        // Empty / tiny grids never engage.
        assert!(beat_loop_region(&[], 0.0, 4, 48_000).is_none());
        assert!(beat_loop_region(&[0.0], 0.0, 1, 48_000).is_none());
        // Zero-length loop is rejected.
        assert!(beat_loop_region(&g, 0.0, 0, 48_000).is_none());
    }
}
