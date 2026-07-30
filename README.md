# livehub

A Rust workspace for playing live: a realtime audio engine, an in-progress DJ/VJ hub, and
**beam** — the small Mac companion for the [Live2play](https://live2play.superduperai.co)
iPhone app.

If you arrived here from Live2play, **[`beam/`](beam) is the part you want.**

## What's in here

| Path | What it is |
|---|---|
| `beam/` | The Mac companion: a one-window app that streams your audio to the phone and remote-controls it. Built with egui. |
| `crates/livehub-engine/` | The realtime runtime — devices, capture, broadcast, discovery, remote-control protocol. Lock-free audio callbacks live here and nowhere else. |
| `crates/livehub-core/` | Shared types. |
| `app/` | The DJ/VJ hub. **Work in progress** — currently closer to a headless harness with an egui surface growing on top. Not the reason this repo is public. |
| `docs/FIELD-SETUP.md` | How to get the Mac and the phone on one network with no WiFi and no SIM. Genuinely useful if you play outside. |

## beam — what it actually does

The iPhone films you and records the take. The problem is the sound: a phone mic under a
good performance is mush. beam fixes that, and then some.

- **Audio to the phone.** Capture *any* input device — a BlackHole loopback carrying Ableton
  Live, REAPER or system audio, or a hardware interface — and stream it over UDP to the
  phone, which writes it into the clip as the audio track.
- **Remote control.** Start/stop recording, flip the camera, drive exposure/ISO/shutter/lens
  from the Mac. Handy when the phone is on a stand across the room.
- **Live preview.** The phone sends frames back so you can see the shot without walking over.
- **Gesture remote.** The phone's gesture engine (mode, profile, note mode, tracking, HUD
  burn-in) is drivable from a sidebar.
- **Tempo.** Relays Ableton Link tempo to the phone, so loops and arp sit on your grid.

Discovery is mDNS, audio is UDP, the gallery is plain HTTP. **Everything is local** — no
internet, no account, no server of ours anywhere in the path.

## Build

Requires a recent Rust toolchain and macOS.

```sh
cargo build --release -p beam
./scripts/install-beam.sh          # assembles beam.app into /Applications
```

The script also compiles `beam/swift/mp-bridge.swift` (the MultipeerConnectivity/AWDL helper)
with `swiftc`; if that step fails, beam still runs and just loses the Multipeer transport.

That flow ad-hoc signs the app so macOS keeps the microphone and local-network grants
between rebuilds. It is **not** notarised, so a copy handed to someone else gets the
Gatekeeper warning — right-click and choose *Open*.

### Shipping a signed build

```sh
./scripts/release-beam.sh        # → dist/beam.zip, signed + notarised + stapled
```

It signs inside-out with Hardened Runtime and `beam/beam.entitlements` (audio-input is
required, or the app records silence under the hardened runtime), notarises through
`asc notarization submit --wait`, staples the ticket and re-zips with `ditto`.

One prerequisite it cannot do for you: a **Developer ID Application** identity in the login
keychain. Apple lets only the *Account Holder* create one — the App Store Connect API
refuses it ("This operation can only be performed by the Account Holder"). Create it in
Xcode → Settings → Accounts → Manage Certificates → **+** → Developer ID Application, or in
the developer portal from a CSR.

## Status and expectations

This is a personal instrument that I use to play, opened up because people asked how the
phone↔Mac side works. It moves when I need it to move. Issues and PRs are welcome, but
there's no roadmap, no support promise, and interfaces change when playing demands it.

## License

MIT — see [LICENSE](LICENSE).
