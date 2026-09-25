//! Minimal wire probe: run the engine's REAL broadcast path (Broadcaster + pump)
//! against a target, bypassing every scrap of beam UI state.
//! `cargo run -p livehub-engine --example broadcast_probe -- 172.20.10.1:9000 MacBook`
fn main() -> anyhow::Result<()> {
    let to: std::net::SocketAddr = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: broadcast_probe <ip:port> [device]"))?
        .parse()?;
    let dev = std::env::args().nth(2).unwrap_or_default();
    if dev == "--raw" {
        // Silent full-rate packets straight through udp_dual/dual_dest — byte-wise
        // what the python control probe sent. Isolates process-level filtering from
        // anything payload- or device-related.
        let sock = livehub_engine::udp_dual(0)?;
        let dest = livehub_engine::dual_dest(to);
        let mut pkt = vec![0u8; 18 + 128 * 8];
        pkt[0..4].copy_from_slice(&0x5243_0100u32.to_le_bytes());
        pkt[12..16].copy_from_slice(&48_000u32.to_le_bytes());
        pkt[16..18].copy_from_slice(&(128u16).to_le_bytes());
        let start = std::time::Instant::now();
        let mut seq = 0u64;
        while start.elapsed() < std::time::Duration::from_secs(8) {
            pkt[4..12].copy_from_slice(&seq.to_le_bytes());
            sock.send_to(&pkt, dest)?;
            seq += 1;
            let target = start + std::time::Duration::from_secs_f64(seq as f64 * 128.0 / 48_000.0);
            if let Some(d) = target.checked_duration_since(std::time::Instant::now()) {
                std::thread::sleep(d);
            }
        }
        eprintln!("raw: sent {seq} packets");
        return Ok(());
    }
    if dev == "--tone" {
        // Device-free: the engine's marker tone through the same udp_dual/dual_dest
        // path — separates "network path broken" from "no mic data to send".
        return livehub_engine::run_broadcast_test(to, 8, false);
    }
    livehub_engine::run_broadcast(&dev, to)
}
