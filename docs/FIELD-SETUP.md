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

## 3. No network at all — Multipeer link activates AWDL, the rig rides it ✅ WORKS
The cleanest field answer, verified working:
1. In Reelcam settings, turn ON **"Multipeer link (no WiFi)"** — its MCSession brings
   up **awdl0** (AWDL = the peer-to-peer Wi-Fi behind AirDrop; no router/internet/SIM).
2. The EXISTING rig rides it automatically: the phone advertises `_reelcam._udp` over
   awdl0 too, beam's mDNS discovery re-finds it there, and the UDP audio + preview flow
   over awdl0. (Hit 🔄 refresh in beam if the address changed.)
3. Turn Wi-Fi fully OFF (Settings → Wi-Fi, NOT Control Center — the latter keeps the
   radio on and reconnects). Audio + preview keep working → confirmed on the link.

Proof it's the link, not Wi-Fi: ping the phone's old Wi-Fi IP — it's dead (100% loss)
while the rig keeps working.

Keep the Multipeer toggle ON in the field — it's what keeps awdl0 alive; without it
AWDL goes dormant and the link drops.

The `mp-bridge` + MCSession data channel (the Rust↔Swift bridge) is the explicit
fallback if the Bonjour-over-AWDL routing ever proves flaky — then audio/gallery get
tunnelled through Multipeer frames directly.

## Not viable
- **Bluetooth**: the uncompressed audio stream is ~3 Mbit/s; Bluetooth tops out near
  1 Mbit/s → it stutters. (Would need an Opus codec + added latency.)
- **AirDrop directly**: closed system, can't carry our traffic (its *underlying* AWDL
  is reachable only via Multipeer Connectivity — see option 3).
