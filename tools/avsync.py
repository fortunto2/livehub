# /// script
# requires-python = ">=3.11"
# dependencies = ["numpy"]
# ///
"""Measure audio-vs-video sync in a reelcam clip recorded during beam's sync test.

beam flashes its whole window white AND fires an audio click at the same instant,
every ~1.5 s. Film that window with reelcam (record the beam/livehub audio), then:

    uv run tools/avsync.py clip.mov          # (or: python3 tools/avsync.py clip.mov)

It finds the white-flash frames (video) and the click transients (audio), pairs
them, and prints the offset = audio_time - video_time. POSITIVE = audio LAGS the
video (the usual case: network/buffer latency). Mean ± jitter across all pulses.
"""

import subprocess
import sys

import numpy as np

AUDIO_SR = 48_000


def run(cmd: list[str]) -> bytes:
    p = subprocess.run(cmd, capture_output=True)
    if p.returncode != 0:
        sys.exit(f"ffmpeg/ffprobe failed:\n{p.stderr.decode(errors='replace')[-800:]}")
    return p.stdout


def video_fps(path: str) -> float:
    out = run([
        "ffprobe", "-v", "0", "-select_streams", "v:0",
        "-show_entries", "stream=r_frame_rate", "-of", "csv=p=0", path,
    ]).decode().strip()
    num, _, den = out.partition("/")
    return float(num) / float(den) if den else float(num)


def frame_luma(path: str) -> np.ndarray:
    """One average-luma byte per video frame (whole frame scaled to 1x1 gray)."""
    raw = run([
        "ffmpeg", "-v", "0", "-i", path,
        "-vf", "scale=1:1,format=gray", "-f", "rawvideo", "-",
    ])
    return np.frombuffer(raw, dtype=np.uint8).astype(np.float32)


def audio_mono(path: str) -> np.ndarray:
    raw = run([
        "ffmpeg", "-v", "0", "-i", path,
        "-map", "0:a:0", "-ac", "1", "-ar", str(AUDIO_SR), "-f", "f32le", "-",
    ])
    return np.frombuffer(raw, dtype=np.float32)


def events(signal: np.ndarray, thresh: float, min_gap: int) -> list[int]:
    """Indices where `signal` first crosses `thresh`, ignoring repeats within
    `min_gap` samples/frames (one event per pulse)."""
    hits = np.flatnonzero(signal > thresh)
    out: list[int] = []
    for i in hits:
        if not out or i - out[-1] > min_gap:
            out.append(int(i))
    return out


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit("usage: avsync.py <clip.mov>")
    path = sys.argv[1]

    fps = video_fps(path)
    luma = frame_luma(path)
    audio = audio_mono(path)
    if luma.size == 0 or audio.size == 0:
        sys.exit("no video frames or no audio track in the clip")

    # Flashes: frames far brighter than the running baseline (median).
    base = float(np.median(luma))
    span = float(luma.max() - base)
    if span < 20:
        sys.exit(f"no clear flashes (luma span {span:.0f}/255) — point the camera at "
                 "beam's window and keep 🎬 sync test ON")
    flash_frames = events(luma, base + span * 0.5, min_gap=int(fps * 0.5))
    flash_times = [f / fps for f in flash_frames]

    # Clicks: sharp audio transients.
    peak = float(np.abs(audio).max())
    if peak < 0.2:
        sys.exit(f"audio too quiet (peak {peak:.2f}) — is beam broadcasting + the "
                 "phone recording the livehub source?")
    click_samples = events(np.abs(audio), peak * 0.5, min_gap=int(AUDIO_SR * 0.5))
    click_times = [s / AUDIO_SR for s in click_samples]

    print(f"clip: {path}")
    print(f"  fps={fps:.2f}  video flashes={len(flash_times)}  audio clicks={len(click_times)}")
    if not flash_times or not click_times:
        sys.exit("need at least one flash AND one click to measure")

    # Pair each flash with the nearest click within 0.75 s.
    offsets = []
    ct = np.array(click_times)
    for ft in flash_times:
        j = int(np.argmin(np.abs(ct - ft)))
        d = click_times[j] - ft
        if abs(d) <= 0.75:
            offsets.append(d * 1000.0)  # ms
            print(f"  flash @ {ft:6.3f}s  →  click @ {click_times[j]:6.3f}s "
                  f" offset {d*1000:+7.1f} ms")

    if not offsets:
        sys.exit("flashes and clicks didn't pair up (offset > 0.75 s) — check the setup")

    a = np.array(offsets)
    print("  ─────────────────────────────────────────────")
    print(f"  A/V offset: mean {a.mean():+.1f} ms   jitter ±{a.std():.1f} ms   (n={len(a)})")
    print("  POSITIVE = audio lags video. Compensate by delaying video by that much,")
    print("  or advance the audio PTS by it.")


if __name__ == "__main__":
    main()
