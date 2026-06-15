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


def duration(path: str, stream: str) -> float:
    out = run([
        "ffprobe", "-v", "0", "-select_streams", stream,
        "-show_entries", "stream=duration", "-of", "csv=p=0", path,
    ]).decode().strip().splitlines()
    try:
        return float(out[0])
    except (IndexError, ValueError):
        return 0.0


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

    # Start-gap: the first beep is the 250ms START marker at tone t=0, so its time
    # IS where the recorded audio begins.
    print(f"  ▶ audio starts at {onsets[0]*1000:6.0f} ms"
          + ("   ⚠ LATE START (silent gap)" if onsets[0] > 0.3 else "   ✓"))

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
    if onsets[0] > 0.3:
        issues.append(f"audio starts {onsets[0]*1000:.0f}ms late")
    if drops:
        issues.append(f"{drops} dropped beep(s)")
    if abs(adur - vdur) > 0.2:
        issues.append(f"audio/video length differ by {abs(adur-vdur):.2f}s")
    print("  " + ("✓ clean: audio from the first sample, beeps 1s apart, lengths match"
                  if not issues else "⚠ " + "; ".join(issues)))


if __name__ == "__main__":
    main()
