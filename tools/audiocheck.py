# /// script
# requires-python = ">=3.11"
# dependencies = ["numpy"]
# ///
"""Diagnose a reelcam clip recorded against `livehub broadcast-test`.

The Mac broadcasts a marker tone — a 250ms 1kHz START beep at t=0, then an 80ms
1kHz beep at every whole second. This finds the beeps in the recording and
reports:
  • where the audio actually STARTS (the start-gap bug: "no audio for 2s"),
  • whether every second's beep is present and ~1.0s apart (dropouts / drift),
  • audio-track vs video-track duration.

    uv run tools/audiocheck.py clip.mov
"""

import subprocess
import sys

import numpy as np

SR = 48_000


def run(cmd: list[str]) -> bytes:
    p = subprocess.run(cmd, capture_output=True)
    if p.returncode != 0:
        sys.exit(f"ffmpeg/ffprobe failed:\n{p.stderr.decode(errors='replace')[-600:]}")
    return p.stdout


def probe(path: str, stream: str, field: str) -> float:
    out = run([
        "ffprobe", "-v", "0", "-select_streams", stream,
        "-show_entries", f"stream={field}", "-of", "csv=p=0", path,
    ]).decode().strip().splitlines()
    try:
        return float(out[0])
    except (IndexError, ValueError):
        return 0.0


def duration(path: str, stream: str) -> float:
    return probe(path, stream, "duration")


def audio_mono(path: str) -> np.ndarray:
    raw = run([
        "ffmpeg", "-v", "0", "-i", path,
        "-map", "0:a:0", "-ac", "1", "-ar", str(SR), "-f", "f32le", "-",
    ])
    return np.frombuffer(raw, dtype=np.float32)


def beep_onsets(audio: np.ndarray, thresh: float, min_gap_s: float) -> list[float]:
    """Start time (s) of each beep — first sample over `thresh`, ignoring repeats
    within `min_gap_s`."""
    hits = np.flatnonzero(np.abs(audio) > thresh)
    out: list[float] = []
    gap = int(min_gap_s * SR)
    for i in hits:
        if not out or i - out[-1] > gap:
            out.append(int(i))
    return [i / SR for i in out]


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit("usage: audiocheck.py <clip.mov>")
    path = sys.argv[1]

    vdur = duration(path, "v:0")
    adur = duration(path, "a:0")
    astart = probe(path, "a:0", "start_time") # where the AUDIO track begins in the video
    audio = audio_mono(path)
    if audio.size == 0:
        sys.exit("no audio track")

    peak = float(np.abs(audio).max())
    if peak < 0.05:
        sys.exit(f"audio is essentially silent (peak {peak:.3f}) — did the stream arrive?")

    onsets = beep_onsets(audio, thresh=peak * 0.4, min_gap_s=0.4)
    audio_len = audio.size / SR

    print(f"clip: {path}")
    print(f"  video {vdur:.2f}s · audio {adur:.2f}s · samples {audio_len:.2f}s · peak {peak:.2f}")
    print(f"  beeps found: {len(onsets)}")
    if not onsets:
        sys.exit("  no beeps detected — wrong tone, or audio didn't record")

    # The REAL start-gap: where the audio TRACK begins in the video timeline
    # (ffprobe start_time). The first-beep time is only its position WITHIN the
    # track, which can hide a leading silent gap.
    print(f"  ▶ audio track starts at {astart*1000:6.0f} ms into the video"
          + ("   ⚠ SILENT START GAP" if astart > 0.1 else "   ✓"))

    # Spacing: beeps should be ~1.0s apart. Gaps ≈ 2s ⇒ a dropped second.
    print("  intervals between beeps (want ~1.000s):")
    drops = 0
    for a, b in zip(onsets, onsets[1:]):
        d = b - a
        flag = ""
        if d > 1.5:
            flag = f"  ⚠ DROPOUT (~{round(d)-1} beep(s) missing)"
            drops += round(d) - 1
        elif abs(d - 1.0) > 0.05:
            flag = "  ⚠ off"
        print(f"    {a:6.2f}s → {b:6.2f}s   {d:5.3f}s{flag}")

    print("  ─────────────────────────────────────────────")
    issues = []
    if astart > 0.1:
        issues.append(f"audio track starts {astart*1000:.0f}ms late (silent start)")
    if drops:
        issues.append(f"{drops} dropped beep(s)")
    # An END gap (audio shorter, but starting at 0) is usually a test artifact (the
    # tone began after / ended before the recording) — flag it, but it's benign for
    # a continuous live stream.
    if astart < 0.1 and adur + 0.15 < vdur:
        issues.append(f"audio ends {vdur-adur:.2f}s before video (likely test pre/post-roll)")
    print("  " + ("✓ clean: audio aligned to the first frame, beeps 1s apart"
                  if not issues else "⚠ " + "; ".join(issues)))


if __name__ == "__main__":
    main()
