//! Crossing detection, algorithm `edge_mid` (spec §8).
//!
//! Per seat, feed band power samples in time order. A crossing starts when the smoothed
//! trace reaches the **enter** level and ends when it drops below the **exit** level. When
//! it ends, the lap time `t_mid` is the midpoint of the two instants the trace crossed the
//! **edge level** (halfway in dB between noise floor and peak, but never below the exit
//! level) on the way up and on the way down. That midpoint is robust to the dip in the
//! middle of a pass caused by the quad antenna's axial null, which a simple peak time is not.
//!
//! A short pre-trigger history is kept so the rising edge can be located even when the edge
//! level is below the enter level (weak passes).

use crate::{Crossing, CrossingFlags};
use oft_nodelink::msg::SampleFlags;

/// How enter/exit levels are chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Threshold {
    /// Relative to the tracked noise floor: enter at `floor + snr_enter`, exit at
    /// `enter - hysteresis`. Both 0.1 dB.
    Auto { snr_enter: i16, hysteresis: i16 },
    /// Absolute levels, 0.1 dB (RotorHazard-style EnterAt / ExitAt).
    Manual { enter: i16, exit: i16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub threshold: Threshold,
    /// Ignore a new crossing starting within this many µs of the previous `t_mid`.
    pub min_lap_us: u64,
    /// Smoothing: `p_s += (p - p_s) >> smooth_shift`. 0 = none.
    pub smooth_shift: u8,
    /// Crossings with SNR below this (0.1 dB) get `LOW_SNR`.
    pub low_snr: i16,
    /// Exit hold-off: a crossing ends only once the trace has stayed below the exit level
    /// for this many µs, so a short dip between the lobes of one pass does not split it.
    /// 0 ends it on the first sample below. The crossing is reported this much later;
    /// its timestamps are not affected.
    pub exit_hold_us: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            threshold: Threshold::Auto { snr_enter: 150, hysteresis: 50 },
            min_lap_us: 2_000_000,
            smooth_shift: 2,
            low_snr: 100,
            exit_hold_us: 100_000,
        }
    }
}

/// Samples kept per crossing, including pre-trigger history. At 1 kHz about 2 s;
/// longer crossings are truncated.
pub const CROSSING_CAP: usize = 2048;
/// Pre-trigger samples copied into the crossing buffer when it starts.
pub const PRE_CAP: usize = 128;
/// Longest hole in the sample stream that is bridged by interpolation, in samples.
pub const MAX_FILL: u64 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Crossing,
}

pub struct Detector {
    cfg: Config,
    seat: u8,
    state: State,
    smoothed: Option<i16>,
    /// Tracked noise floor in 1/256 of 0.1 dB (fixed point, so slow rates don't truncate
    /// to zero). Starts from the first sample.
    floor_fp: Option<i32>,
    last_mid: Option<u64>,
    /// Set when min-lap suppressed a rising edge; the rest of that pulse is ignored
    /// until the trace falls below the exit level.
    suppressed: bool,
    /// Previous sample time, to learn the sample period.
    prev_t: Option<u64>,
    prev_power: i16,
    /// Nominal sample period. Holes in the stream do not change it.
    dt_us: u64,
    dt_known: bool,
    /// Consecutive intervals that looked like holes; a long run means the rate really changed.
    gap_run: u8,
    // Pre-trigger ring of smoothed samples.
    pre: [i16; PRE_CAP],
    pre_next: usize,
    pre_count: usize,
    // Current crossing.
    buf: [i16; CROSSING_CAP],
    len: usize,
    /// Node time of `buf[0]`.
    t_first: u64,
    /// Index in `buf` of the sample that crossed the enter level.
    enter_idx: usize,
    peak: i16,
    /// Floor frozen at crossing start, 0.1 dB.
    floor_at_enter: i16,
    /// While in a crossing: time of the first sample of the current run below the exit
    /// level, if the trace is below it now.
    below_since: Option<u64>,
    flags: CrossingFlags,
}

impl Detector {
    pub fn new(seat: u8, cfg: Config) -> Self {
        Self {
            cfg,
            seat,
            state: State::Idle,
            smoothed: None,
            floor_fp: None,
            last_mid: None,
            suppressed: false,
            prev_t: None,
            prev_power: 0,
            dt_us: 1000,
            dt_known: false,
            gap_run: 0,
            pre: [0; PRE_CAP],
            pre_next: 0,
            pre_count: 0,
            buf: [0; CROSSING_CAP],
            len: 0,
            t_first: 0,
            enter_idx: 0,
            peak: i16::MIN,
            floor_at_enter: 0,
            below_since: None,
            flags: CrossingFlags::empty(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn set_config(&mut self, cfg: Config) {
        self.cfg = cfg;
    }

    /// Current noise floor estimate, 0.1 dB, if any sample has been seen.
    pub fn floor(&self) -> Option<i16> {
        self.floor_fp.map(|f| (f >> 8) as i16)
    }

    /// Current smoothed power, 0.1 dB.
    pub fn smoothed(&self) -> Option<i16> {
        self.smoothed
    }

    /// Current enter and exit levels, 0.1 dB.
    pub fn levels(&self) -> (i16, i16) {
        match self.cfg.threshold {
            Threshold::Manual { enter, exit } => (enter, exit),
            Threshold::Auto { snr_enter, hysteresis } => {
                let f = self.floor().unwrap_or(0);
                let enter = f.saturating_add(snr_enter);
                (enter, enter.saturating_sub(hysteresis))
            }
        }
    }

    /// Whether a crossing is in progress.
    pub fn is_crossing(&self) -> bool {
        self.state == State::Crossing
    }

    /// Let the Node's own noise-floor estimate nudge ours (only while idle).
    pub fn floor_hint(&mut self, node_floor: i16) {
        if self.state == State::Idle {
            let hint = (node_floor as i32) << 8;
            self.floor_fp = Some(match self.floor_fp {
                None => hint,
                Some(f) => f + (hint - f) / 4,
            });
        }
    }

    /// Feed one sample. `t` is on the **Node clock**; the returned crossing is on the Node
    /// clock too. Map with [`crate::ClockMap`] afterwards.
    ///
    /// Samples are expected at a steady rate. A hole (lost batches) of up to [`MAX_FILL`]
    /// samples is filled by linear interpolation, so the buffers stay evenly spaced and a
    /// hole inside a pass neither splits it nor shifts its timestamps. A longer outage ends
    /// a crossing in progress as `TRUNCATED`.
    pub fn push(&mut self, t: u64, power: i16, flags: SampleFlags) -> Option<Crossing> {
        let mut out = None;
        if let Some(prev) = self.prev_t {
            if t > prev {
                let d = t - prev;
                if !self.dt_known || d < 2 * self.dt_us || self.gap_run >= 8 {
                    self.dt_us = d;
                    self.dt_known = true;
                    self.gap_run = 0;
                } else {
                    self.gap_run += 1;
                    let missing = (d + self.dt_us / 2) / self.dt_us - 1;
                    if missing <= MAX_FILL {
                        let (a, b) = (self.prev_power as i64, power as i64);
                        for k in 1..=missing {
                            let p = a + (b - a) * k as i64 / (missing as i64 + 1);
                            let c = self.step(prev + k * self.dt_us, p as i16, SampleFlags::empty());
                            out = out.or(c);
                        }
                    } else {
                        self.pre_count = 0;
                        if self.state == State::Crossing {
                            self.flags.set(CrossingFlags::TRUNCATED);
                            out = Some(self.finish(self.below_since.unwrap_or(prev)));
                        }
                    }
                }
            }
        }
        self.prev_t = Some(t);
        self.prev_power = power;
        let c = self.step(t, power, flags);
        out.or(c)
    }

    fn step(&mut self, t: u64, power: i16, flags: SampleFlags) -> Option<Crossing> {
        let s = match self.smoothed {
            Some(p) if self.cfg.smooth_shift > 0 => p + ((power as i32 - p as i32) >> self.cfg.smooth_shift) as i16,
            _ => power,
        };
        self.smoothed = Some(s);

        let floor_fp = *self.floor_fp.get_or_insert((s as i32) << 8);
        let floor = (floor_fp >> 8) as i16;
        let (enter, exit) = self.levels();

        match self.state {
            State::Idle => {
                // Track the floor only while the signal is well below the enter level, so a
                // pass's shoulders don't lift it. Faster downwards than upwards.
                let quiet_below = ((floor as i32 + enter as i32) / 2) as i16;
                if s < quiet_below {
                    let diff = ((s as i32) << 8) - floor_fp;
                    let step = if diff < 0 { diff / 64 } else { diff / 1024 };
                    self.floor_fp = Some(floor_fp + step);
                }

                if self.suppressed {
                    if s < exit {
                        self.suppressed = false;
                    }
                    self.push_pre(s);
                    return None;
                }
                if s >= enter {
                    if let Some(last) = self.last_mid {
                        if t.saturating_sub(last) < self.cfg.min_lap_us {
                            self.suppressed = true;
                            self.push_pre(s);
                            return None;
                        }
                    }
                    self.start_crossing(t, s, floor, flags);
                } else {
                    self.push_pre(s);
                }
                None
            }
            State::Crossing => {
                if s > self.peak {
                    self.peak = s;
                }
                self.store(s, flags);
                if s < exit {
                    // The crossing ends where the trace first went below the exit level,
                    // but only once it has stayed there for the hold-off.
                    let since = *self.below_since.get_or_insert(t);
                    if t.saturating_sub(since) >= self.cfg.exit_hold_us {
                        return Some(self.finish(since));
                    }
                } else {
                    self.below_since = None;
                }
                if self.len >= CROSSING_CAP {
                    self.flags.set(CrossingFlags::TRUNCATED);
                    return Some(self.finish(self.below_since.unwrap_or(t)));
                }
                None
            }
        }
    }

    fn push_pre(&mut self, s: i16) {
        self.pre[self.pre_next] = s;
        self.pre_next = (self.pre_next + 1) % PRE_CAP;
        if self.pre_count < PRE_CAP {
            self.pre_count += 1;
        }
    }

    fn start_crossing(&mut self, t: u64, s: i16, floor: i16, flags: SampleFlags) {
        self.state = State::Crossing;
        self.flags = CrossingFlags::empty();
        self.peak = s;
        self.floor_at_enter = floor;
        self.below_since = None;
        // Copy pre-trigger history, oldest first.
        self.len = 0;
        let start = (self.pre_next + PRE_CAP - self.pre_count) % PRE_CAP;
        for k in 0..self.pre_count {
            self.buf[self.len] = self.pre[(start + k) % PRE_CAP];
            self.len += 1;
        }
        self.enter_idx = self.len;
        self.t_first = t.saturating_sub(self.len as u64 * self.dt_us);
        self.store(s, flags);
        self.pre_count = 0;
    }

    fn store(&mut self, s: i16, flags: SampleFlags) {
        if self.len < CROSSING_CAP {
            self.buf[self.len] = s;
            self.len += 1;
        }
        if flags.contains(SampleFlags::CLIPPED) {
            self.flags.set(CrossingFlags::CLIPPED);
        }
        if flags.contains(SampleFlags::POST_HOP) {
            self.flags.set(CrossingFlags::HOP_ARTIFACT);
        }
    }

    fn finish(&mut self, t_exit: u64) -> Crossing {
        self.state = State::Idle;
        let floor = self.floor_at_enter;
        let peak = self.peak;
        let snr = peak.saturating_sub(floor);
        if snr < self.cfg.low_snr {
            self.flags.set(CrossingFlags::LOW_SNR);
        }
        let (_, exit) = self.levels();
        let edge = (((floor as i32 + peak as i32) / 2) as i16).max(exit);
        let n = self.len;
        let dt = self.dt_us as f64;

        // Dips below the edge shorter than the hold-off belong to the pass, on both flanks.
        let hold = (self.cfg.exit_hold_us / self.dt_us.max(1)) as usize;
        // Rise: where the trace came up through the edge on its way into this crossing,
        // interpolated from the sample before. Search from the enter sample, not from the
        // start of the pre-trigger history: an earlier, separate bump is not this pass.
        let e = self.enter_idx.min(n - 1);
        let i = if self.buf[e] >= edge {
            let mut i = e;
            let mut below = 0;
            for k in (0..e).rev() {
                if self.buf[k] >= edge {
                    i = k;
                    below = 0;
                } else {
                    below += 1;
                    if below > hold {
                        break;
                    }
                }
            }
            i
        } else {
            // The edge is above the enter level (a strong pass): the rise comes later.
            (e..n).find(|&i| self.buf[i] >= edge).unwrap_or(e)
        };
        let t_rise = if i == 0 {
            0.0
        } else {
            let (a, b) = (self.buf[i - 1] as f64, self.buf[i] as f64);
            (i - 1) as f64 + if b > a { (edge as f64 - a) / (b - a) } else { 1.0 }
        };
        // Fall: last index at or above the edge, interpolated to the sample after.
        let j = (0..n).rev().find(|&j| self.buf[j] >= edge).unwrap_or(n - 1);
        let t_fall = if j + 1 >= n {
            j as f64
        } else {
            let (a, b) = (self.buf[j] as f64, self.buf[j + 1] as f64);
            j as f64 + if a > b { (a - edge as f64) / (a - b) } else { 0.0 }
        };
        // The EMA delays both edges by (2^shift − 1) samples; take that back out so
        // t_mid is on the signal, not on the filtered trace.
        let lag = ((1u32 << self.cfg.smooth_shift) - 1) as f64;
        let mid = self.t_first as f64 + dt * ((t_rise + t_fall) / 2.0 - lag);
        let t_mid = if mid < 0.0 { 0 } else { mid as u64 };
        self.last_mid = Some(t_mid);
        Crossing {
            seat: self.seat,
            t_enter: self.t_first + self.enter_idx as u64 * self.dt_us,
            t_exit,
            t_mid,
            peak,
            snr,
            flags: self.flags,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    #[test]
    fn levels_follow_threshold_mode() {
        let mut d = Detector::new(0, Config::default());
        d.floor_hint(-900);
        assert_eq!(d.levels(), (-750, -800));
        d.set_config(Config { threshold: Threshold::Manual { enter: -700, exit: -760 }, ..Config::default() });
        assert_eq!(d.levels(), (-700, -760));
    }

    #[test]
    fn min_lap_ignores_the_whole_pulse_not_just_its_start() {
        // Two identical 300 ms pulses, 1.9 s apart, min lap 2 s. The second pulse straddles
        // the min-lap boundary; none of it may become a crossing.
        let cfg = Config { threshold: Threshold::Manual { enter: -750, exit: -800 }, smooth_shift: 0, ..Config::default() };
        let mut d = Detector::new(0, cfg);
        let mut found = Vec::new();
        let pulse = |t: u64, c: u64| -> i16 { if t.abs_diff(c) < 150_000 { -600 } else { -920 } };
        let mut t = 0;
        while t < 4_000_000 {
            if let Some(c) = d.push(t, pulse(t, 1_000_000).max(pulse(t, 2_900_000)), SampleFlags::empty()) {
                found.push(c);
            }
            t += 1000;
        }
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].t_mid.abs_diff(1_000_000) < 2_000);
        // A third pulse well after min lap is detected again.
        while t < 7_000_000 {
            if let Some(c) = d.push(t, pulse(t, 6_000_000), SampleFlags::empty()) {
                found.push(c);
            }
            t += 1000;
        }
        assert_eq!(found.len(), 2);
    }

    /// A 300 ms flat-topped pulse centred on `c`, on a quiet floor.
    fn pulse(t: u64, c: u64) -> i16 {
        if t.abs_diff(c) < 150_000 { -600 } else { -920 }
    }

    fn manual(exit_hold_us: u64) -> Config {
        Config { threshold: Threshold::Manual { enter: -750, exit: -800 }, smooth_shift: 0, min_lap_us: 0, exit_hold_us, ..Config::default() }
    }

    #[test]
    fn a_short_dip_does_not_split_a_pass_with_exit_hold_off() {
        // One pass with a 40 ms drop to the floor in the middle.
        let signal = |t: u64| if t.abs_diff(1_000_000) < 20_000 { -920 } else { pulse(t, 1_000_000) };
        let run = |hold| {
            let mut d = Detector::new(0, manual(hold));
            (0..2_000u64).filter_map(|k| d.push(k * 1000, signal(k * 1000), SampleFlags::empty())).collect::<Vec<_>>()
        };
        assert_eq!(run(0).len(), 2, "without hold-off the dip splits the pass");
        let held = run(100_000);
        assert_eq!(held.len(), 1);
        // Timestamps describe the pass, not the hold-off: it ended when the signal fell.
        assert!(held[0].t_mid.abs_diff(1_000_000) < 2_000, "{:?}", held[0]);
        assert!(held[0].t_exit.abs_diff(1_150_000) < 2_000, "{:?}", held[0]);
    }

    #[test]
    fn a_bump_before_the_pass_does_not_move_its_time() {
        // A separate bump above the exit level 100 ms before the pass, still inside the
        // pre-trigger history, is not the pass's rising edge.
        let signal = |t: u64| if (750_000..770_000).contains(&t) { -780 } else { pulse(t, 1_000_000) };
        let mut d = Detector::new(0, manual(0));
        let found: Vec<_> = (0..2_000u64).filter_map(|k| d.push(k * 1000, signal(k * 1000), SampleFlags::empty())).collect();
        assert_eq!(found.len(), 1);
        assert!(found[0].t_mid.abs_diff(1_000_000) < 2_000, "{:?}", found[0]);
    }

    #[test]
    fn a_hole_in_the_stream_does_not_split_or_shift_a_pass() {
        // 60 ms of samples lost just before the pass starts, and 60 ms lost inside it.
        let lost = |t: u64| (790_000..850_000).contains(&t) || (1_020_000..1_080_000).contains(&t);
        let mut d = Detector::new(0, manual(100_000));
        let found: Vec<_> = (0..2_000u64).map(|k| k * 1000).filter(|&t| !lost(t)).filter_map(|t| d.push(t, pulse(t, 1_000_000), SampleFlags::empty())).collect();
        assert_eq!(found.len(), 1);
        let c = found[0];
        // The rising edge fell inside the first hole, so it is only known to within the hole.
        assert!(c.t_mid.abs_diff(1_000_000) < 32_000, "{c:?}");
        assert!(c.t_enter >= 790_000 && c.t_enter <= 851_000, "{c:?}");
        assert!(c.t_exit.abs_diff(1_150_000) < 2_000, "{c:?}");
        assert_eq!(d.dt_us, 1000, "a hole is not the sample period");
    }

    #[test]
    fn a_long_outage_ends_a_crossing_as_truncated() {
        let mut d = Detector::new(0, manual(100_000));
        let mut found = Vec::new();
        for t in (0..1_000u64).map(|k| k * 1000).chain((2_000..2_500u64).map(|k| k * 1000)) {
            found.extend(d.push(t, pulse(t, 1_000_000), SampleFlags::empty()));
        }
        assert_eq!(found.len(), 1);
        assert!(found[0].flags.contains(CrossingFlags::TRUNCATED));
        assert!(found[0].t_exit <= 1_000_000);
    }

    #[test]
    fn sample_period_is_learned_from_the_stream() {
        let mut d = Detector::new(0, Config::default());
        d.push(0, -900, SampleFlags::empty());
        d.push(500, -900, SampleFlags::empty());
        assert_eq!(d.dt_us, 500);
    }
}
