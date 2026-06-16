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
let myPeer = MCPeerID(displayName: (Host.current().localizedName ?? "beam").prefix(60).description)
let mcSession = MCSession(peer: myPeer, securityIdentity: nil, encryptionPreference: .none)

func note(_ s: String) {
    FileHandle.standardError.write((s + "\n").data(using: .utf8)!)
}

final class Bridge: NSObject, MCSessionDelegate, MCNearbyServiceBrowserDelegate {
    let browser = MCNearbyServiceBrowser(peer: myPeer, serviceType: serviceType)
    let outLock = NSLock()

    override init() {
        super.init()
        mcSession.delegate = self
        browser.delegate = self
        browser.startBrowsingForPeers()
        note("browsing for reelcam…")
    }

    // Browser: invite any reelcam peer we find.
    func browser(_ b: MCNearbyServiceBrowser, foundPeer peerID: MCPeerID,
                 withDiscoveryInfo info: [String: String]?) {
        note("found \(peerID.displayName)")
        b.invitePeer(peerID, to: mcSession, withContext: nil, timeout: 20)
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
            if buf.count < 4 + len { break }
            let payload = Data(buf[4 ..< (4 + len)])
            buf.removeFirst(4 + len)
            if !mcSession.connectedPeers.isEmpty {
                try? mcSession.send(payload, toPeers: mcSession.connectedPeers, with: .reliable)
            }
        }
    }
    exit(0)
}

RunLoop.main.run()
