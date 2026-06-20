/// mp-bridge — MultipeerConnectivity transport for beam, bridged to Rust over pipes.
///
/// AWDL peer-to-peer Wi-Fi (the tech behind AirDrop) → connects the Mac ↔ iPhone with
/// NO router, internet, or SIM. beam (Rust) spawns this and talks to it over stdio:
///   • stdin  (Rust → peer): length-prefixed frames `[u32 LE len][payload]` → MCSession.
///   • stdout (peer → Rust): the same framing for data received from the phone.
///   • stderr: text status lines (`state … connected`) for the Rust side to surface.
///
/// Build: swiftc -O -framework MultipeerConnectivity -framework Foundation \
///          beam/swift/mp-bridge.swift -o mp-bridge

import Foundation
import MultipeerConnectivity

let serviceType = "reelcam-mp" // must match reelcam's advertiser
let mpToken = "rcm-mp-7f3a"    // must match reelcam — only our beam may connect
let myPeer = MCPeerID(displayName: (Host.current().localizedName ?? "beam").prefix(60).description)
let mcSession = MCSession(peer: myPeer, securityIdentity: nil, encryptionPreference: .none)

func note(_ s: String) {
    FileHandle.standardError.write((s + "\n").data(using: .utf8)!)
}

final class Bridge: NSObject, MCSessionDelegate, MCNearbyServiceBrowserDelegate {
    let browser = MCNearbyServiceBrowser(peer: myPeer, serviceType: serviceType)
    let outLock = NSLock()
    var lastPeer: MCPeerID? // for re-inviting if a connect attempt stalls/drops

    override init() {
        super.init()
        mcSession.delegate = self
        browser.delegate = self
        browser.startBrowsingForPeers()
        note("browsing for reelcam…")
        // Auto-recovery: while NOT connected, restart browsing every few seconds so a
        // dropped link re-discovers + re-invites the (still-advertising) phone on its
        // own — no manual Direct off/on. (Runs on the main RunLoop, kept alive below.)
        Timer.scheduledTimer(withTimeInterval: 4, repeats: true) { [weak self] _ in
            guard let self, mcSession.connectedPeers.isEmpty else { return }
            note("re-browsing…")
            browser.stopBrowsingForPeers()
            browser.startBrowsingForPeers()
            if let p = lastPeer { invite(p) }
        }
    }

    private func invite(_ peerID: MCPeerID) {
        // Short timeout so a stalled connect retries fast (e.g. while Local Network
        // permission is still being granted), instead of hanging ~20 s.
        browser.invitePeer(peerID, to: mcSession, withContext: mpToken.data(using: .utf8), timeout: 10)
    }

    // Browser: invite any reelcam peer we find.
    func browser(_ b: MCNearbyServiceBrowser, foundPeer peerID: MCPeerID,
                 withDiscoveryInfo info: [String: String]?) {
        lastPeer = peerID
        note("found \(peerID.displayName)")
        invite(peerID)
    }
    func browser(_ b: MCNearbyServiceBrowser, lostPeer peerID: MCPeerID) {
        note("lost \(peerID.displayName)")
    }
    func browser(_ b: MCNearbyServiceBrowser, didNotStartBrowsingForPeers error: Error) {
        note("browse error: \(error.localizedDescription)")
    }

    // Session state → stderr.
    func session(_ s: MCSession, peer: MCPeerID, didChange state: MCSessionState) {
        let st = state == .connected ? "connected" : (state == .connecting ? "connecting" : "notconnected")
        note("state \(peer.displayName) \(st)")
        // A stalled/dropped connect (often while the Local Network grant settles) →
        // retry shortly so it recovers without a manual restart.
        if state == .notConnected, let p = lastPeer {
            DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [weak self] in
                guard let self, mcSession.connectedPeers.isEmpty else { return }
                note("retry invite \(p.displayName)")
                self.invite(p)
            }
        }
    }

    // Data from the phone → stdout (framed).
    func session(_ s: MCSession, didReceive data: Data, fromPeer peerID: MCPeerID) {
        var len = UInt32(data.count).littleEndian
        outLock.lock()
        FileHandle.standardOutput.write(Data(bytes: &len, count: 4))
        FileHandle.standardOutput.write(data)
        outLock.unlock()
    }
    func session(_: MCSession, didReceive _: InputStream, withName _: String, fromPeer _: MCPeerID) {}
    func session(_: MCSession, didStartReceivingResourceWithName _: String, fromPeer _: MCPeerID, with _: Progress) {}
    func session(_: MCSession, didFinishReceivingResourceWithName _: String, fromPeer _: MCPeerID, at _: URL?, withError _: Error?) {}
}

let bridge = Bridge()

// stdin reader: parse `[u32 len][payload]` frames, forward each to the connected peer.
DispatchQueue.global(qos: .userInteractive).async {
    let stdin = FileHandle.standardInput
    var buf = [UInt8]()
    while true {
        let chunk = stdin.availableData
        if chunk.isEmpty { break } // EOF → beam closed
        buf.append(contentsOf: chunk)
        while buf.count >= 4 {
            let len = Int(buf[0]) | Int(buf[1]) << 8 | Int(buf[2]) << 16 | Int(buf[3]) << 24
            if buf.count < 4 + len || len < 1 { break }
            // Frame = [u32 len][u8 mode][payload]. mode: 0 unreliable, 1 reliable, 2 file.
            let mode = buf[4]
            let payload = Data(buf[5 ..< (4 + len)])
            buf.removeFirst(4 + len)
            guard !mcSession.connectedPeers.isEmpty else { continue }
            if mode == 2 {
                // Resource: payload = "path\tname" → send the file to every peer.
                if let s = String(data: payload, encoding: .utf8) {
                    let parts = s.components(separatedBy: "\t")
                    if parts.count == 2 {
                        let url = URL(fileURLWithPath: parts[0])
                        for peer in mcSession.connectedPeers {
                            mcSession.sendResource(at: url, withName: parts[1], toPeer: peer,
                                                   withCompletionHandler: { e in
                                if let e = e { note("resource error: \(e.localizedDescription)") }
                                else { note("resource sent \(parts[1])") }
                            })
                        }
                    }
                }
            } else {
                try? mcSession.send(payload, toPeers: mcSession.connectedPeers,
                                    with: mode != 0 ? .reliable : .unreliable)
            }
        }
    }
    exit(0)
}

RunLoop.main.run()
