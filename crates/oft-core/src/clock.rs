//! Node clock → Timer clock mapping (spec §4.2).
//!
//! Both clocks are µs counters that drift relative to each other by tens of ppm.
//! We fit `t_timer = offset + (1 + drift) * t_node` by least squares over a sliding
//! window of observations. Observations come from:
//!
//! - **SYNC edges**: the Timer knows when it pulsed (`t_timer`), the Node reports the
//!   Node time it saw the edge (`t_node`). Near-perfect pairs.
//! - **Ping/pong**: `t_node` paired with the midpoint of the round trip. Only the lowest
//!   round trips are worth keeping; the caller filters with [`ClockMap::add_ping`].

/// Observations kept in the fit window.
pub const WINDOW: usize = 64;

#[derive(Debug, Clone, Copy)]
pub struct ClockMap {
    obs: [(u64, u64); WINDOW],
    n: usize,
    next: usize,
    // Fit, relative to the first observation to keep the arithmetic well conditioned.
    base_node: u64,
    base_timer: u64,
    offset_us: f64,
    slope: f64,
    residual_us: f64,
    /// Lowest ping round trip seen, µs. Pings slower than 2× this are discarded.
    best_rtt_us: u64,
}

impl Default for ClockMap {
    fn default() -> Self {
        Self::new()
    }
}

impl ClockMap {
    pub const fn new() -> Self {
        Self {
            obs: [(0, 0); WINDOW],
            n: 0,
            next: 0,
            base_node: 0,
            base_timer: 0,
            offset_us: 0.0,
            slope: 1.0,
            residual_us: 0.0,
            best_rtt_us: u64::MAX,
        }
    }

    /// True once at least two observations have been fitted.
    pub fn is_synced(&self) -> bool {
        self.n >= 2
    }

    /// Number of observations in the window.
    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// RMS residual of the fit, µs. A rough estimate of mapping error.
    pub fn residual_us(&self) -> f64 {
        self.residual_us
    }

    /// Node drift relative to the Timer, parts per million. Positive = Node clock runs fast.
    pub fn drift_ppm(&self) -> f64 {
        (1.0 / self.slope - 1.0) * 1e6
    }

    /// Add an exact pair: a SYNC edge the Timer emitted at `t_timer` and the Node saw at `t_node`.
    pub fn add_pair(&mut self, t_node: u64, t_timer: u64) {
        if self.n == 0 {
            self.base_node = t_node;
            self.base_timer = t_timer;
        }
        self.obs[self.next] = (t_node, t_timer);
        self.next = (self.next + 1) % WINDOW;
        if self.n < WINDOW {
            self.n += 1;
        }
        self.refit();
    }

    /// Add a ping observation. `t_sent`/`t_recv` are Timer times of the ping and the pong,
    /// `t_node` is the Node time inside the pong. Returns whether it was kept.
    pub fn add_ping(&mut self, t_sent: u64, t_recv: u64, t_node: u64) -> bool {
        let rtt = t_recv.saturating_sub(t_sent);
        if rtt < self.best_rtt_us {
            self.best_rtt_us = rtt;
        } else if rtt > self.best_rtt_us.saturating_mul(2) {
            return false;
        }
        self.add_pair(t_node, t_sent + rtt / 2);
        true
    }

    /// Map a Node time onto the Timer clock. Before sync, returns `t_node` unchanged.
    pub fn to_timer(&self, t_node: u64) -> u64 {
        if self.n == 0 {
            return t_node;
        }
        let x = t_node as f64 - self.base_node as f64;
        let y = self.offset_us + self.slope * x + self.base_timer as f64;
        if y < 0.0 {
            0
        } else {
            y as u64
        }
    }

    fn refit(&mut self) {
        let n = self.n as f64;
        let (mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0);
        for &(tn, tt) in &self.obs[..self.n] {
            let x = tn as f64 - self.base_node as f64;
            let y = tt as f64 - self.base_timer as f64;
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
        }
        let denom = n * sxx - sx * sx;
        if self.n < 2 || denom.abs() < 1e-9 {
            // One point, or all at the same instant: offset only.
            self.slope = 1.0;
            self.offset_us = (sy - sx) / n;
        } else {
            self.slope = (n * sxy - sx * sy) / denom;
            self.offset_us = (sy - self.slope * sx) / n;
            // Guard against absurd fits from a bad early sample: clocks don't drift 1%.
            if (self.slope - 1.0).abs() > 0.01 {
                self.slope = 1.0;
                self.offset_us = (sy - sx) / n;
            }
        }
        let mut ss = 0.0;
        for &(tn, tt) in &self.obs[..self.n] {
            let x = tn as f64 - self.base_node as f64;
            let y = tt as f64 - self.base_timer as f64;
            let r = y - (self.offset_us + self.slope * x);
            ss += r * r;
        }
        self.residual_us = sqrt(ss / n);
    }
}

/// Newton square root; `core` has no `f64::sqrt` without `std`.
fn sqrt(v: f64) -> f64 {
    if v <= 0.0 {
        return 0.0;
    }
    let mut x = v;
    for _ in 0..40 {
        x = 0.5 * (x + v / x);
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Node clock that runs `ppm` fast and started `offset` µs after the Timer.
    /// Panics if the Node clock would be negative: that is a test-setup error.
    fn node_time(t_timer: u64, offset: i64, ppm: f64) -> u64 {
        let v = (t_timer as f64) * (1.0 + ppm * 1e-6) - offset as f64;
        assert!(v >= 0.0, "node clock negative at t_timer={t_timer}");
        v as u64
    }

    #[test]
    fn unsynced_is_identity() {
        let m = ClockMap::new();
        assert!(!m.is_synced());
        assert_eq!(m.to_timer(12345), 12345);
    }

    #[test]
    fn sync_edges_fit_offset_and_drift() {
        let mut m = ClockMap::new();
        let (offset, ppm) = (5_000_000, 30.0);
        // 10 Hz edges for 5 s.
        for k in 0..50u64 {
            let tt = 10_000_000 + k * 100_000;
            m.add_pair(node_time(tt, offset, ppm), tt);
        }
        assert!(m.is_synced());
        assert!((m.drift_ppm() - ppm).abs() < 2.0, "drift {}", m.drift_ppm());
        // Map a time 1 s past the last edge: extrapolation error must stay tiny.
        let tt = 16_000_000;
        let mapped = m.to_timer(node_time(tt, offset, ppm));
        assert!((mapped as i64 - tt as i64).abs() <= 2, "mapped {mapped} vs {tt}");
    }

    #[test]
    fn pings_keep_only_fast_round_trips() {
        let mut m = ClockMap::new();
        let (offset, ppm) = (-250_000, -12.0);
        let mut kept = 0;
        for k in 0..40u64 {
            let t_sent = 100_000 + k * 50_000;
            // Every 4th ping is slow (USB stall).
            let rtt = if k % 4 == 3 { 9_000 } else { 400 + (k % 3) * 20 };
            let t_node = node_time(t_sent + rtt / 2, offset, ppm);
            if m.add_ping(t_sent, t_sent + rtt, t_node) {
                kept += 1;
            }
        }
        assert!((28..40).contains(&kept), "kept {kept}");
        let tt = 2_500_000;
        let mapped = m.to_timer(node_time(tt, offset, ppm));
        assert!((mapped as i64 - tt as i64).abs() <= 30, "mapped {mapped} vs {tt}");
        assert!(m.residual_us() < 30.0);
    }

    #[test]
    fn window_slides() {
        let mut m = ClockMap::new();
        for k in 0..(WINDOW as u64 * 3) {
            let tt = k * 100_000;
            m.add_pair(node_time(tt, 0, 0.0), tt);
        }
        assert_eq!(m.len(), WINDOW);
        assert!(m.residual_us() < 1.0);
    }
}
