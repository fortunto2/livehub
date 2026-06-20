//! disc — print what beam's mDNS Discovery actually resolves (diagnose "doesn't find
//! the phone"). Usage: cargo run -p beam --bin disc

use livehub_engine::Discovery;

fn main() {
    let d = Discovery::start().expect("discovery start");
    println!("browsing _reelcam._udp for 15 s …");
    for i in 1..=15 {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let peers = d.peers();
        let list: Vec<String> =
            peers.iter().map(|p| format!("{}@{}", p.name, p.addr)).collect();
        println!("[{i:>2}s] {} peer(s): {:?}", peers.len(), list);
    }
}
