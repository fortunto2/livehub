//! beam-rx — a diagnostic receiver. Plays beam's UDP audio stream on THIS Mac (the same
//! wire format the phone gets) and logs packet rate / sequence gaps. It isolates the
//! crackle: if rx plays clean, the source (beam / the aggregate device) is fine and the
//! problem is the phone's receive/playback; if rx crackles too, it's upstream.
//!
//! Usage:  cargo run -p beam --bin rx [port]      (default 9999)
//! Then in beam set  Phone (ip:port) = 127.0.0.1:<port>  and hit ▶ broadcast.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::net::UdpSocket;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const MAGIC: u32 = 0x5243_0100;
const HEADER: usize = 18; // magic u32 + seq u64 + sr u32 + n_frames u16

fn push_samples(prod: &mut rtrb::Producer<f32>, bytes: &[u8]) {
    let mut i = 0;
    while i + 4 <= bytes.len() {
        let s = f32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        let _ = prod.push(s); // drop on overflow
        i += 4;
    }
}

fn main() {
    let port: u16 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(9999);
    let sock = UdpSocket::bind(("0.0.0.0", port)).expect("bind");
    println!("beam-rx on 0.0.0.0:{port}  →  in beam set Phone = 127.0.0.1:{port}, hit ▶ broadcast");

    let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(48_000 * 2); // ~1 s stereo
    let mut buf = [0u8; 4096];

    // Wait for the first valid packet to learn the sample rate.
    let sr = loop {
        let Ok((n, _)) = sock.recv_from(&mut buf) else { continue };
        if n > HEADER && u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) == MAGIC {
            let sr = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
            push_samples(&mut prod, &buf[HEADER..n]);
            break sr;
        }
    };
    println!("stream sr = {sr} Hz — playing on the default output (build a small buffer first)…");

    let host = cpal::default_host();
    let dev = host.default_output_device().expect("no default output");
    let cfg = cpal::StreamConfig {
        channels: 2,
        sample_rate: cpal::SampleRate(sr),
        buffer_size: cpal::BufferSize::Default,
    };
    let underruns = Arc::new(AtomicU64::new(0));
    let cb_under = underruns.clone();
    let stream = dev
        .build_output_stream(
            &cfg,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for s in out.iter_mut() {
                    match cons.pop() {
                        Ok(v) => *s = v,
                        Err(_) => {
                            *s = 0.0;
                            cb_under.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            },
            |e| eprintln!("output error: {e}"),
            None,
        )
        .expect("output stream");
    stream.play().unwrap();

    let mut last_seq: Option<u64> = None;
    let mut packets = 0u64;
    let mut gaps = 0u64;
    let start = std::time::Instant::now();
    let mut last_print = std::time::Instant::now();
    loop {
        let Ok((n, _)) = sock.recv_from(&mut buf) else { continue };
        if n <= HEADER || u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) != MAGIC {
            continue;
        }
        let seq = u64::from_le_bytes(buf[4..12].try_into().unwrap());
        if let Some(ls) = last_seq {
            if seq > ls + 1 {
                gaps += seq - ls - 1; // missing packets between ls and seq
            }
        }
        last_seq = Some(seq);
        packets += 1;
        push_samples(&mut prod, &buf[HEADER..n]);
        if last_print.elapsed().as_secs_f32() >= 1.0 {
            let secs = start.elapsed().as_secs_f32().max(0.001);
            println!(
                "pkts {packets} ({:.0}/s) · seq-gaps {gaps} (loss) · underruns {} (playback gaps) · {:.1}s",
                packets as f32 / secs,
                underruns.load(Ordering::Relaxed),
                secs,
            );
            last_print = std::time::Instant::now();
        }
    }
}
