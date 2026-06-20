# Future spec (ТЗ): multicam montage + live streaming

**Status: planning, not built.** Captures two ideas discussed for the
beam (Mac) + reelcam (iPhone) rig, the cheap foundations to lay down *now*
(while still single-phone), and the open decisions. Nothing here changes the
working rig until explicitly picked up.

Anchor decisions already made by the user:
- Montage happens **on the computer** (beam/livehub), not the phone.
- Phones are the **angle source**; their recorded audio is a **sync reference**,
  the final audio bed comes from beam's clean master mix.
- A **single phone clip must stay Instagram-ready right now** (vertical 1080,
  compatible codec, AAC) — straight to Insta with no Mac step.
- For streaming, **YouTube is enough** for now.

---

## Idea 1 — Multicam montage (edit on the Mac)

### Goal
2+ iPhones each film an angle of the same DJ set; assemble a vertical reel on
the Mac by cutting between angles, locked to the music (beatgrid). Not built
yet — but lay the foundations now so today's single-phone clips are forward-
compatible.

### Core insight: shared audio = free frame-accurate sync
beam already sends the **identical clean mix** to every phone. If each phone
records that audio, the angles align in post by cross-correlating the identical
waveform (~1 ms precision, well under a frame). This is the "sync by audio" that
Final Cut / PluralEyes charge for — we get it for free.

Even better: **beam records the master mix locally** (lossless, cleaner than the
phone copy). Then the final reel uses **beam's master as the audio bed**, and the
phones' audio is only the alignment reference.

### Pipeline
1. beam records the master mix + a timecode log per session.
2. Each phone ISO-records its full angle (+ its copy of the mix for sync).
3. Post (on the Mac): align phone videos to beam's master via audio
   cross-correlation → all angles on one timeline.
4. Cut between Cam1/Cam2 — manual, or auto-on-beat using the planned livehub
   beatgrid (`video_audio::detect_beats`, already owned).
5. Render the vertical reel: chosen angles + beam's clean master audio.

### Two modes (can support both)
- **Live director-cut (PGM):** 2-up monitor in beam, keys 1/2 pick the live
  angle; only that feed lands in the reel. Instant, low storage, but committed
  live.
- **ISO + post montage** (this is "мультикамерный монтаж"): both record full
  angles, cut later. Flexible; this is where beat-locked auto-montage shines.
- **Hybrid:** live-log cut points (tap 1/2 during the set → timecoded cut list)
  then finish in post. "Marked live, finished later."

### Lay down NOW (single phone, forward-compatible, invisible to the Insta flow)
The one thing that **cannot be retrofitted** is per-clip timing. Stamp it now:
- **Clip metadata stamp:** `session_id`, camera role (default `cam1`), **exact
  record-start time + offset from beam's clock**, fps, sample rate, audio source.
  Store as a filename convention + a tiny JSON sidecar the file-server serves
  next to the clip. (You can't add a record-start timecode to a clip filmed last
  month.)
- **Clock handshake at record-start:** beam sends its monotonic clock over the
  existing control channel → phone stores the offset in the clip metadata. Unused
  with one phone; the coarse sync anchor later (audio xcorr is the fine align).
- **Keep recording the livehub mix as the clip's audio track** — it is both the
  Instagram-ready sound *now* and the sync reference *later*. Already so; don't
  regress.
- **(soon) beam records the master mix per session** — clean archive now; the
  montage audio bed + sync anchor later.

### Milestones
- **M-a (cheap, proves it):** ISO-record both (manual start) → pull both via the
  gallery → align by shared audio → export a synced multicam (FCPXML / synced
  media) → cut in Final Cut / Resolve. New code only: clip tagging + xcorr offset
  + export. Reuses broadcast + gallery + AWDL.
- **M-b:** "record all" from beam (one key, both roll) + master-mix record on
  beam + 2-up live monitor + live cut-logging.
- **M-c:** in-app beat-locked auto-montage (ties to livehub beatgrid + the wgpu
  video compositor, livehub M3–M4) → render the reel without leaving the rig.

### Gotchas
- AWDL with 3 peers (Mac + 2 phones) + 2 audio sends + 2 previews: Multipeer
  allows up to 8 peers, but in the field a **5 GHz travel router** is steadier.
- **Thermal:** a phone doing 4K/60 + preview streaming + WiFi gets hot — drop
  preview bitrate/res during ISO recording.
- A self-built multicam editor is the big video lift; **exporting to Final
  Cut/Resolve is far faster to value.**

---

## Idea 2 — Live streaming (YouTube)

### Platform reality
- **YouTube Live:** open **RTMP**, easy, vertical OK — stream from phone or Mac.
- **Instagram:** has **Live Producer (web, RTMP)** but with caveats (account
  eligibility, limits, horizontal-leaning); the in-app vertical Live still takes
  the **phone mic** — software can't inject clean audio into another app's mic on
  iOS. Clean-audio IG Live = a hardware audio-interface-as-mic route, a separate
  track. **Don't architect on unofficial IG RTMP.**
- **TikTok:** RTMP only via LIVE Studio for eligible accounts.
- **YouTube is the target.**

### Hard constraint: streaming needs real internet
AWDL / the no-SIM field rig is **local-only** — no uplink, no stream. Live is for
venues **with** internet (venue WiFi, or a router with a SIM). The field rig stays
"record now, post later."

### Clean audio is the edge → it decides where to stream from
The stream must carry the **clean board mix**, not room mic. That mix already
lives in **beam** — which is the single biggest argument for streaming from the
Mac (zero extra audio plumbing, perfect sync).

### Quality: double compression (the key question)
Codec generations on each path:
- **Phone → YouTube direct:** 2 encodes (phone + YouTube's own re-encode).
- **Phone → Mac → YouTube:** **3 encodes** — the Mac adds one. (YouTube always
  re-encodes the input to VP9/AV1 regardless of path.)

Conclusion:
- **Single camera, plain YouTube, no overlays → stream straight from the phone.**
  Fewer encodes = better picture *and* simpler. Routing through the Mac only adds
  a generation of loss for no benefit here.
- **The Mac (beam-as-OBS) earns its extra encode only when it must decode to
  composite** (overlays, scenes, 2-camera switching). Then mitigate: phone feeds
  beam at a **fat bitrate** (1080p @ 20–30 Mbit/s, well above YouTube's needs) so
  the intermediate is visually transparent; the only quality-defining step is the
  final beam→YouTube encode (~8–12 Mbit/s). Standard broadcast practice.

### Two architectures
- **Phone-direct RTMP (fast win):** reelcam encodes + pushes RTMP to YouTube;
  clean audio already on the phone; native vertical; add `HaishinKit` + a
  stream-key field. Can **stream and record the file simultaneously** (keep the
  postable reel). Downsides: phone thermal under sustained load; no compositing /
  multicam / overlays.
- **beam-as-OBS (strategic):** beam encodes + pushes; clean master audio already
  there; overlays / scenes / brand; later **multi-angle switching**; one RTMP out
  → YouTube now, IG Live Producer / TikTok LIVE Studio where eligible, or a
  restreamer (Restream.io) to fan out. Needs a **real phone→beam video feed**
  (today only a low-res MJPEG preview exists).

### Recommendation
- **Now / single-cam YouTube:** phone-direct RTMP.
- **Growth (overlays, scenes, multicam):** beam-as-OBS.

### Convergence
The strategic streaming path and the multicam path need the **same** upgrade — a
**real (stream-quality) video feed from the phone(s) into beam**, replacing the
MJPEG preview. Build that once; it serves both live streaming and multicam
montage. Streaming is additive — it does not conflict with the
record-to-Insta or montage-on-Mac foundations above.

---

## Shared foundations (serve both ideas)
1. **Real phone→beam video feed** (H.264/HEVC, stream quality) — replaces the
   MJPEG preview; the single biggest shared enabler.
2. **Session id + record-start timecode + clock handshake** in clip metadata —
   the un-retrofittable bit; lay down now.
3. **beam records the master mix** per session — clean audio bed + sync anchor.
4. **Keep single-phone clips Instagram-ready** (vertical 1080, compatible codec,
   AAC); 4K/HEVC stays an opt-in "best source for Mac montage."

## Idea 3 — Backup mode: beam records clean audio, merge in post

The field link (AWDL) can crackle/drop. This decouples audio QUALITY from the
live link: **beam records the clean source locally** (lossless, no network
artifacts); the live link only carries **start/stop sync + preview**, not the
audio that ends up in the reel. Crackle stops mattering.

### Flow
1. beam records the broadcast source to a **WAV** when recording (synced to the
   record button), stamped with `session_id` + start time. (M1)
2. The phone records video (+ its copy of the live stream as a sync reference).
3. Pull the video to beam (gallery / Direct tunnel). (M2)
4. **ffmpeg** muxes beam's clean WAV onto the phone video, offset-aligned. (M2)

### Sync by transients (handles network delays)
A constant start-offset fails if the link added variable delay. Align by
**spikes**, with multiple anchors:
- **Injected click** (precise): fire beam's A/V-sync `pulse()` at record start —
  it lands in both beam's WAV (known, t=0) and the phone's reference. Detect the
  click in the reference → exact offset. Survives crackle (a click is a huge
  transient). Periodic clicks → piecewise warp for long takes.
- **Natural transients** (no click): onset-detect (spectral flux) the loud hits
  in both signals, match the strongest, align by them. Uses the music's own
  spikes.
The crackly reference is used ONLY to locate the click/onsets — the final audio
is beam's clean WAV. Reuses `video_audio::detect_beats` (onsets) + `pulse()`.

### Milestones
- **M1** (foundation, building now): beam writes a clean WAV synced to the record
  button + a start click as the sync anchor + session metadata.
- **M2:** pull video + ffmpeg merge with a coarse offset.
- **M3:** auto transient/click alignment (+ multi-anchor for long takes).

## Open decisions
- Streaming first cut: phone-direct (fast) vs beam-as-OBS (strategic)?
- Orientation priority: vertical (reels/IG/TikTok) vs horizontal (classic
  YouTube) — sets the whole pipeline's aspect.
- Multicam editor: export to Final Cut/Resolve (fast) vs in-app beat montage
  (self-contained, bigger build)?
