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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            threshold: Threshold::Auto { snr_enter: 150, hysteresis: 50 },
            min_lap_us: 2_000_000,
            smooth_shift: 2,
            low_snr: 100,
        }
    }
}

/// Samples kept per crossing, including pre-trigger history. At 1 kHz about 2 s;
/// longer crossings are truncated.
pub const CROSSING_CAP: usize = 2048;
/// Pre-trigger samples copied into the crossing buffer when it starts.
pub const PRE_CAP: usize = 128;

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
    /// Previous sample time, to learn the sample period.
    prev_t: Option<u64>,
    dt_us: u64,
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
            prev_t: None,
            dt_us: 1000,
            pre: [0; PRE_CAP],
            pre_next: 0,
            pre_count: 0,
            buf: [0; CROSSING_CAP],
            len: 0,
            t_first: 0,
            enter_idx: 0,
            peak: i16::MIN,
            floor_at_enter: 0,
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
    pub fn push(&mut self, t: u64, power: i16, flags: SampleFlags) -> Option<Crossing> {
        if let Some(prev) = self.prev_t {
            if t > prev {
                self.dt_us = t - prev;
            }
        }
        self.prev_t = Some(t);

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

                if s >= enter {
                    if let Some(last) = self.last_mid {
                        if t.saturating_sub(last) < self.cfg.min_lap_us {
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
                    return Some(self.finish(t));
                }
                if self.len >= CROSSING_CAP {
                    self.flags.set(CrossingFlags::TRUNCATED);
                    return Some(self.finish(t));
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

        // Rise: first index at or above the edge, interpolated from the sample before.
        let i = (0..n).find(|&i| self.buf[i] >= edge).unwrap_or(self.enter_idx);
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

    #[test]
    fn levels_follow_threshold_mode() {
        let mut d = Detector::new(0, Config::default());
        d.floor_hint(-900);
        assert_eq!(d.levels(), (-750, -800));
        d.set_config(Config { threshold: Threshold::Manual { enter: -700, exit: -760 }, ..Config::default() });
        assert_eq!(d.levels(), (-700, -760));
    }

    #[test]
    fn sample_period_is_learned_from_the_stream() {
        let mut d = Detector::new(0, Config::default());
        d.push(0, -900, SampleFlags::empty());
        d.push(500, -900, SampleFlags::empty());
        assert_eq!(d.dt_us, 500);
    }
}
