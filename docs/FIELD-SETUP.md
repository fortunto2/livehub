# Field setup — connecting beam (Mac) ↔ reelcam (iPhone) with no WiFi / no SIM

The whole rig is **local-only** — audio (UDP), discovery (mDNS), the gallery (HTTP).
**No internet is needed.** All that's required is the Mac and the iPhone on one local
network. The iPhone can't host (no SIM → no Personal Hotspot), so the Mac hosts, or
you bring a network.

Options, simplest first:

## 1. Any Wi-Fi network (easiest)
Join the Mac and iPhone to the same Wi-Fi (a venue's, a phone hotspot, a cheap
battery **travel router** ~$15 — needs no internet, just makes a local LAN). Done.
In beam, the phone's IP may change → hit **🔄 refresh**.

## 2. Mac hosts Wi-Fi via Internet Sharing — no extra hardware
A single Wi-Fi adapter **cannot** be both the source and the access point, and Apple
removed the old single-adapter Host-AP API. The trick: share from **Thunderbolt
Bridge** (a built-in virtual interface, always present — nothing needs to be plugged
into it) **to Wi-Fi**.

1. System Settings → General → Sharing → **Internet Sharing** → ⓘ Configure.
2. **Share your connection from:** Thunderbolt Bridge.
3. **To devices using:** check **Wi-Fi** → Wi-Fi Options → set a name + WPA2 password.
4. Turn **Internet Sharing** ON (it warns there's no internet — fine, we only need the
   local network).
5. On the iPhone: Settings → Wi-Fi → join the Mac's network.
6. Both are now on the Mac's subnet (~192.168.x). In beam hit **🔄 refresh**.

If macOS refuses to enable the Wi-Fi option with Thunderbolt Bridge (some builds want
a "live" source), the only no-internet fallback is a second interface — a cheap
**USB-C→Ethernet adapter** as the source (it need not be plugged into anything).

## 3. No network at all — Multipeer Connectivity (a dev project, not a setting)
AirDrop/AirPlay run over **AWDL** (Apple Wireless Direct Link) = direct device-to-
device Wi-Fi with **no router, no internet, no SIM**, fast enough for the rig. The
public API is **Multipeer Connectivity**. reelcam is Swift (easy); beam is Rust, so it
needs a Swift bridge or the transport (UDP audio + HTTP gallery) re-done over Multipeer
sessions. This is the truly infrastructure-free answer if the field has nothing.

## Not viable
- **Bluetooth**: the uncompressed audio stream is ~3 Mbit/s; Bluetooth tops out near
  1 Mbit/s → it stutters. (Would need an Opus codec + added latency.)
- **AirDrop directly**: closed system, can't carry our traffic (its *underlying* AWDL
  is reachable only via Multipeer Connectivity — see option 3).
