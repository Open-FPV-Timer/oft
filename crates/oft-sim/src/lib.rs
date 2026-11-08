//! Synthetic gate passes for testing detection and clock mapping without hardware.
//!
//! The model (all in dB, then converted to 0.1 dB integers):
//!
//! ```text
//! power(t) = floor + noise(σ) + Σ pass_k(t)
//! pass(t)  = height · exp(−(t−center)²/(2·width²)) · (1 − dip · exp(−(t−center)²/(2·(width/4)²)))
//! ```
//!
//! The second factor carves the axial-null dip into the middle of the pulse. Output is
//! clipped at `clip` with the `CLIPPED` flag set, as a saturated receiver would.

pub mod fake_node;

use oft_nodelink::msg::{SampleFlags, Samples, MAX_SAMPLES};

/// Tiny deterministic PRNG (xorshift64*). Same seed, same signal.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in [0, 1).
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// Standard normal (Box–Muller).
    pub fn normal(&mut self) -> f64 {
        let u1 = self.uniform().max(1e-12);
        let u2 = self.uniform();
        (-2.0 * u1.ln()).sqrt() * (2.0 * core::f64::consts::PI * u2).cos()
    }
}

/// One synthetic pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pass {
    /// Centre of the pulse, µs on the Node clock. This is the "true" lap time.
    pub center_us: u64,
    /// Peak height above the floor, dB.
    pub height_db: f64,
    /// Gaussian width (σ), µs. A 25 mW whoop at walking pace is ~100–200 ms.
    pub width_us: f64,
    /// Depth of the axial-null dip, 0..1. 0 = none, 0.5 = halves the power at the centre.
    pub dip: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    /// Noise floor, dB.
    pub floor_db: f64,
    /// Noise standard deviation, dB.
    pub noise_db: f64,
    /// Receiver saturation level, dB. Samples above it are clipped and flagged.
    pub clip_db: f64,
    /// Sample period, µs.
    pub dt_us: u64,
    pub passes: Vec<Pass>,
}

impl Default for Scene {
    fn default() -> Self {
        Self { floor_db: -92.0, noise_db: 0.8, clip_db: -20.0, dt_us: 1000, passes: Vec::new() }
    }
}

impl Scene {
    /// Power in dB at Node time `t`, before noise and clipping.
    pub fn clean_db(&self, t: u64) -> f64 {
        let mut p = self.floor_db;
        for pass in &self.passes {
            let x = t as f64 - pass.center_us as f64;
            let main = (-(x * x) / (2.0 * pass.width_us * pass.width_us)).exp();
            let w4 = pass.width_us / 4.0;
            let null = 1.0 - pass.dip * (-(x * x) / (2.0 * w4 * w4)).exp();
            p += pass.height_db * main * null;
        }
        p
    }

    /// One sample: (power in 0.1 dB, flags).
    pub fn sample(&self, t: u64, rng: &mut Rng) -> (i16, SampleFlags) {
        let db = self.clean_db(t) + self.noise_db * rng.normal();
        if db >= self.clip_db {
            ((self.clip_db * 10.0).round() as i16, SampleFlags::CLIPPED)
        } else {
            ((db * 10.0).round() as i16, SampleFlags::empty())
        }
    }

    /// Generate `Samples` batches covering `[t_start, t_end)`, as a Node would send them.
    pub fn batches(&self, seat: u8, t_start: u64, t_end: u64, per_batch: usize, rng: &mut Rng) -> Vec<Samples> {
        let per_batch = per_batch.clamp(1, MAX_SAMPLES);
        let floor = (self.floor_db * 10.0).round() as i16;
        let mut out = Vec::new();
        let mut t = t_start;
        while t < t_end {
            let mut s = Samples::new(seat, t, self.dt_us as u16, floor, SampleFlags::empty());
            let mut flags = SampleFlags::empty();
            for _ in 0..per_batch {
                if t >= t_end {
                    break;
                }
                let (p, f) = self.sample(t, rng);
                flags.0 |= f.0;
                s.push(p).unwrap();
                t += self.dt_us;
            }
            s.flags = flags;
            out.push(s);
        }
        out
    }
}

/// A simulated Node clock: runs `ppm` fast relative to the Timer and started `offset_us` later.
#[derive(Debug, Clone, Copy)]
pub struct NodeClock {
    pub offset_us: i64,
    pub ppm: f64,
}

impl NodeClock {
    /// Panics if the Node clock would be negative: use a smaller offset or a later start.
    pub fn node_time(&self, t_timer: u64) -> u64 {
        let v = (t_timer as f64) * (1.0 + self.ppm * 1e-6) - self.offset_us as f64;
        assert!(v >= 0.0, "NodeClock: node time negative at t_timer={t_timer} (offset {})", self.offset_us);
        v as u64
    }
    pub fn timer_time(&self, t_node: u64) -> u64 {
        ((t_node as f64 + self.offset_us as f64) / (1.0 + self.ppm * 1e-6)).max(0.0) as u64
    }
}
