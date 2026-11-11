//! Detection replayed over signal recorded from real hardware (`tests/data/`).
//!
//! The recording is eight hand-carried passes with lumpy, multi-lobe shapes and holes in the
//! sample stream. There is no ground truth for the pass centre, so these tests check what a
//! Race Manager needs first: every pass that reaches the enter level is one crossing, never
//! several, and its timestamp lies inside the pass.

use oft_core::{Crossing, DetectConfig, Detector, Threshold};
use oft_nodelink::msg::SampleFlags;

/// Times (µs) the signal was at its strongest in each of the eight passes, read off the trace.
const PASSES_US: [u64; 8] = [1_056_150_000, 1_068_340_000, 1_076_410_000, 1_084_350_000, 1_092_300_000, 1_097_170_000, 1_105_100_000, 1_110_450_000];

fn samples() -> Vec<(u64, i16)> {
    let text = include_str!("data/hand-passes-2026-11-11.txt");
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
        let mut it = line.split_ascii_whitespace();
        let t0: u64 = it.next().unwrap().parse().unwrap();
        let dt: u64 = it.next().unwrap().parse().unwrap();
        out.extend(it.enumerate().map(|(k, p)| (t0 + k as u64 * dt, p.parse::<i16>().unwrap())));
    }
    out
}

fn run(cfg: DetectConfig) -> Vec<Crossing> {
    let mut det = Detector::new(0, cfg);
    samples().into_iter().filter_map(|(t, p)| det.push(t, p, SampleFlags::empty())).collect()
}

fn manual(enter: i16, exit: i16, exit_hold_us: u64) -> DetectConfig {
    // No min-lap, so a split pass shows up as extra crossings instead of being hidden.
    DetectConfig { threshold: Threshold::Manual { enter, exit }, min_lap_us: 0, exit_hold_us, ..DetectConfig::default() }
}

/// For each pass, the crossings whose span comes within 0.7 s of its strongest point.
fn per_pass(found: &[Crossing]) -> Vec<Vec<Crossing>> {
    PASSES_US.iter().map(|&p| found.iter().copied().filter(|c| c.t_enter < p + 700_000 && c.t_exit + 700_000 > p).collect()).collect()
}

#[test]
#[ignore]
fn print_replay() {
    for (enter, exit) in [(-260, -330), (-300, -380), (-330, -400)] {
        for hold in [0u64, 60_000, 100_000, 150_000] {
            let found = run(manual(enter, exit, hold));
            let pp = per_pass(&found);
            println!("enter {enter} exit {exit} hold {:3} ms: total {:2}  per pass {:?}", hold / 1000, found.len(), pp.iter().map(|v| v.len()).collect::<Vec<_>>());
            if hold == 100_000 {
                for (p, v) in PASSES_US.iter().zip(&pp) {
                    for c in v {
                        println!("    pass {:.3}: enter {:+5} ms  mid {:+5} ms  exit {:+5} ms  peak {}", *p as f64 / 1e6, (c.t_enter as i64 - *p as i64) / 1000, (c.t_mid as i64 - *p as i64) / 1000, (c.t_exit as i64 - *p as i64) / 1000, c.peak);
                    }
                }
            }
        }
    }
    let auto = run(DetectConfig { min_lap_us: 0, ..DetectConfig::default() });
    println!("auto default: total {} per pass {:?}", auto.len(), per_pass(&auto).iter().map(|v| v.len()).collect::<Vec<_>>());
}

/// Timestamps must describe the pass: in order, and `t_mid` no earlier than the pre-trigger
/// history reaches (128 samples before `t_enter`).
fn assert_sane(found: &[Crossing]) {
    for c in found {
        assert!(c.t_enter < c.t_exit, "{c:?}");
        assert!(c.t_mid + 130_000 >= c.t_enter && c.t_mid <= c.t_exit, "{c:?}");
    }
    assert!(found.windows(2).all(|w| w[0].t_exit <= w[1].t_enter), "crossings overlap");
}

#[test]
fn without_hold_off_the_lobes_of_a_pass_become_separate_crossings() {
    let found = run(manual(-300, -380, 0));
    assert!(found.len() > 20, "expected the recording to fragment, got {}", found.len());
    assert_sane(&found);
}

#[test]
fn default_hold_off_gives_one_crossing_per_pass() {
    let hold = DetectConfig::default().exit_hold_us;
    // Levels the recording was made with: five passes reach the enter level.
    let found = run(manual(-260, -330, hold));
    assert_sane(&found);
    assert_eq!(per_pass(&found).iter().map(|v| v.len()).collect::<Vec<_>>(), [1, 1, 1, 0, 0, 1, 0, 1]);
    assert_eq!(found.len(), 5);
    // Levels low enough for all eight.
    let found = run(manual(-330, -400, hold));
    assert_sane(&found);
    assert_eq!(per_pass(&found).iter().map(|v| v.len()).collect::<Vec<_>>(), [1; 8]);
}

#[test]
fn holes_in_the_stream_do_not_corrupt_timestamps() {
    // Before holes were bridged, a hole next to a crossing made its times land seconds away
    // (the hole was taken for the sample period). Every setting must stay sane.
    for (enter, exit) in [(-260, -330), (-300, -380), (-330, -400)] {
        for hold in [0, 60_000, 100_000, 150_000] {
            assert_sane(&run(manual(enter, exit, hold)));
        }
    }
    let found = run(manual(-300, -380, 150_000));
    assert_eq!(per_pass(&found).iter().map(|v| v.len()).collect::<Vec<_>>(), [1; 8]);
}
