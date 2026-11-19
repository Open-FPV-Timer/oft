//! Replay a recording made by `bench/record.mjs` through the detector and print the crossings.
//!   cargo run -p oftd --example replay -- <file.jsonl> [snr_enter] [hysteresis] [min_lap_ms] [exit_hold_ms] [from_s] [to_s] [pass_window_ms]
//! Levels in 0.1 dB. The Node's floor report in each batch is used as in the Timer.
use oft_core::{DetectConfig, Detector, Threshold};
use oft_nodelink::msg::SampleFlags;
use std::io::BufRead;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let num = |i: usize, d: f64| a.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    let cfg = DetectConfig {
        threshold: Threshold::Auto { snr_enter: num(2, 220.0) as i16, hysteresis: num(3, 50.0) as i16 },
        min_lap_us: (num(4, 1500.0) * 1000.0) as u64,
        exit_hold_us: (num(5, 80.0) * 1000.0) as u64,
        pass_window_us: (num(8, 0.0) * 1000.0) as u64,
        ..DetectConfig::default()
    };
    let (from, to) = ((num(6, 0.0) * 1e6) as u64, (num(7, 1e12) * 1e6) as u64);
    let mut det = Detector::new(0, cfg);
    let mut last: Option<u64> = None;
    for line in std::io::BufReader::new(std::fs::File::open(&a[1]).expect("open")).lines() {
        let Ok(m) = serde_json::from_str::<serde_json::Value>(&line.unwrap()) else { continue };
        if m["type"] != "samples" {
            continue;
        }
        let (t0, dt) = (m["t0"].as_u64().unwrap(), m["dt_us"].as_u64().unwrap());
        if t0 < from || t0 > to {
            continue;
        }
        if let Some(nf) = m["nf"].as_i64() {
            if nf > -3000 {
                det.floor_hint(nf as i16);
            }
        }
        for (k, p) in m["p"].as_array().unwrap().iter().enumerate() {
            if let Some(c) = det.push(t0 + k as u64 * dt, p.as_i64().unwrap() as i16, SampleFlags::empty()) {
                let lap = last.map(|l| format!("{:7.3}", (c.t_mid - l) as f64 / 1e6)).unwrap_or_else(|| "       ".into());
                println!("{:9.3} lap {lap}  dur {:5} ms  peak {:6.1}  snr {:5.1}", c.t_mid as f64 / 1e6, (c.t_exit - c.t_enter) / 1000, c.peak as f64 / 10.0, c.snr as f64 / 10.0);
                last = Some(c.t_mid);
            }
        }
    }
}
