//! Detection against synthetic passes: does `edge_mid` find every pass, and how close is
//! `t_mid` to the true centre?

use oft_core::{Crossing, CrossingFlags, DetectConfig, Detector, Threshold};
use oft_sim::{Pass, Rng, Scene};

fn run(scene: &Scene, cfg: DetectConfig, t_end: u64, seed: u64) -> Vec<Crossing> {
    let mut rng = Rng::new(seed);
    let mut det = Detector::new(0, cfg);
    let mut out = Vec::new();
    for batch in scene.batches(0, 0, t_end, 50, &mut rng) {
        det.floor_hint(batch.noise_floor);
        for (k, &p) in batch.power().iter().enumerate() {
            if let Some(c) = det.push(batch.time_of(k), p, batch.flags) {
                out.push(c);
            }
        }
    }
    out
}

fn passes_every(n: usize, start_us: u64, gap_us: u64, height_db: f64, width_us: f64, dip: f64) -> Vec<Pass> {
    (0..n).map(|k| Pass { center_us: start_us + k as u64 * gap_us, height_db, width_us, dip }).collect()
}

#[test]
fn clean_passes_are_found_within_a_few_ms() {
    let scene = Scene { passes: passes_every(10, 3_000_000, 5_000_000, 35.0, 120_000.0, 0.0), ..Scene::default() };
    let found = run(&scene, DetectConfig::default(), 60_000_000, 1);
    assert_eq!(found.len(), 10, "found {:?}", found.iter().map(|c| c.t_mid).collect::<Vec<_>>());
    for (c, p) in found.iter().zip(&scene.passes) {
        let err = c.t_mid as i64 - p.center_us as i64;
        assert!(err.abs() < 2_500, "t_mid off by {err} µs");
        assert!(c.snr > 300, "snr {}", c.snr);
        assert_eq!(c.flags, CrossingFlags::empty());
    }
}

#[test]
fn axial_null_dip_does_not_split_a_pass_or_move_its_time() {
    // A deep dip (70 %) in the middle of every pass.
    let scene = Scene { passes: passes_every(8, 3_000_000, 6_000_000, 40.0, 150_000.0, 0.7), ..Scene::default() };
    let found = run(&scene, DetectConfig::default(), 60_000_000, 2);
    assert_eq!(found.len(), 8, "dip split passes: {}", found.len());
    for (c, p) in found.iter().zip(&scene.passes) {
        let err = c.t_mid as i64 - p.center_us as i64;
        assert!(err.abs() < 3_000, "t_mid off by {err} µs with dip");
    }
}

#[test]
fn clipped_pass_is_flagged_and_still_timed() {
    let scene = Scene {
        clip_db: -60.0, // floor -92 + 50 dB pass → clips hard
        passes: passes_every(3, 2_000_000, 4_000_000, 50.0, 120_000.0, 0.0),
        ..Scene::default()
    };
    let found = run(&scene, DetectConfig::default(), 14_000_000, 3);
    assert_eq!(found.len(), 3);
    for (c, p) in found.iter().zip(&scene.passes) {
        assert!(c.flags.contains(CrossingFlags::CLIPPED));
        let err = c.t_mid as i64 - p.center_us as i64;
        assert!(err.abs() < 3_000, "t_mid off by {err} µs while clipped");
    }
}

#[test]
fn min_lap_suppresses_a_bounce() {
    // Two pulses 800 ms apart: a real pass and a bounce. Only one lap.
    let scene = Scene {
        passes: vec![
            Pass { center_us: 2_000_000, height_db: 35.0, width_us: 100_000.0, dip: 0.0 },
            Pass { center_us: 2_800_000, height_db: 30.0, width_us: 100_000.0, dip: 0.0 },
            Pass { center_us: 8_000_000, height_db: 35.0, width_us: 100_000.0, dip: 0.0 },
        ],
        ..Scene::default()
    };
    let found = run(&scene, DetectConfig::default(), 12_000_000, 4);
    assert_eq!(found.len(), 2, "bounce was counted: {:?}", found.iter().map(|c| c.t_mid).collect::<Vec<_>>());
}

#[test]
fn noise_alone_produces_no_crossings() {
    let scene = Scene { noise_db: 2.0, ..Scene::default() };
    let found = run(&scene, DetectConfig::default(), 120_000_000, 5);
    assert!(found.is_empty(), "false crossings: {}", found.len());
}

#[test]
fn weak_pass_is_flagged_low_snr_or_missed_but_never_mistimed() {
    let scene = Scene { passes: passes_every(5, 3_000_000, 5_000_000, 16.0, 120_000.0, 0.0), ..Scene::default() };
    let cfg = DetectConfig { threshold: Threshold::Auto { snr_enter: 100, hysteresis: 40 }, low_snr: 200, ..Default::default() };
    let found = run(&scene, cfg, 30_000_000, 6);
    for c in &found {
        assert!(c.flags.contains(CrossingFlags::LOW_SNR));
        let nearest = scene.passes.iter().map(|p| (c.t_mid as i64 - p.center_us as i64).abs()).min().unwrap();
        assert!(nearest < 8_000, "weak pass mistimed by {nearest} µs");
    }
}

#[test]
fn manual_thresholds_match_rotorhazard_style() {
    let scene = Scene { passes: passes_every(4, 3_000_000, 5_000_000, 35.0, 120_000.0, 0.0), ..Scene::default() };
    // Floor -92 dB → enter at -75 dB, exit at -80 dB.
    let cfg = DetectConfig { threshold: Threshold::Manual { enter: -750, exit: -800 }, ..Default::default() };
    let found = run(&scene, cfg, 25_000_000, 7);
    assert_eq!(found.len(), 4);
}

#[test]
fn floor_tracks_a_slow_drift() {
    // Floor rises 6 dB over a minute; auto thresholds must follow and not false-trigger.
    let mut scene = Scene { passes: passes_every(3, 10_000_000, 20_000_000, 35.0, 120_000.0, 0.0), ..Scene::default() };
    let mut rng = Rng::new(8);
    let mut det = Detector::new(0, DetectConfig::default());
    let mut found = 0;
    let dt = scene.dt_us;
    let mut t = 0;
    while t < 70_000_000 {
        scene.floor_db = -92.0 + 6.0 * (t as f64 / 60_000_000.0).min(1.0);
        let (p, f) = scene.sample(t, &mut rng);
        if det.push(t, p, f).is_some() {
            found += 1;
        }
        t += dt;
    }
    assert_eq!(found, 3);
    let floor = det.floor().unwrap();
    assert!((floor as i32 - (-860)).abs() < 20, "floor {floor}");
}

#[test]
#[ignore]
fn print_margins() {
    for (name, h, w, dip, clip) in [("clean", 35.0, 120_000.0, 0.0, -20.0), ("dip", 40.0, 150_000.0, 0.7, -20.0), ("clip", 50.0, 120_000.0, 0.0, -60.0), ("weak", 16.0, 120_000.0, 0.0, -20.0), ("wide", 35.0, 300_000.0, 0.3, -20.0), ("narrow", 35.0, 40_000.0, 0.3, -20.0)] {
        let scene = Scene { clip_db: clip, passes: passes_every(10, 3_000_000, 5_000_000, h, w, dip), ..Scene::default() };
        let cfg = if name == "weak" { DetectConfig { threshold: Threshold::Auto { snr_enter: 100, hysteresis: 40 }, ..Default::default() } } else { DetectConfig::default() };
        let found = run(&scene, cfg, 60_000_000, 11);
        let errs: Vec<i64> = found.iter().map(|c| scene.passes.iter().map(|p| c.t_mid as i64 - p.center_us as i64).min_by_key(|e| e.abs()).unwrap()).collect();
        let max = errs.iter().map(|e| e.abs()).max().unwrap_or(0);
        let mean = if errs.is_empty() { 0 } else { errs.iter().sum::<i64>() / errs.len() as i64 };
        eprintln!("{name:7} found {:2}/10  max|err| {max:6} µs  mean err {mean:6} µs  errs {errs:?}", found.len());
    }
}
