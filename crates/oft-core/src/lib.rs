//! OFT core (spec §4, §8). Runs unchanged on Linux, the RP2350 and inside Grid.
//!
//! - [`clock`]: maps a Node's µs clock onto the Timer clock from SYNC edges or ping/pong.
//! - [`detect`]: turns a stream of band-power samples into [`Crossing`]s (`edge_mid`).
//!
//! Power values are **0.1 dB** integers, the same as on the Node link.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod clock;
pub mod detect;
pub mod host;

pub use clock::ClockMap;
pub use detect::{Config as DetectConfig, Detector, Threshold};
pub use host::{Event, NodeHost};

/// One pass of a quad through the gate. Times are µs; see the producer for which clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crossing {
    pub seat: u8,
    /// First sample at or above the enter level.
    pub t_enter: u64,
    /// First sample below the exit level after the peak.
    pub t_exit: u64,
    /// The lap timestamp: midpoint of the edge-level crossings on the rise and the fall.
    pub t_mid: u64,
    /// Peak smoothed power, 0.1 dB.
    pub peak: i16,
    /// Peak minus noise floor, 0.1 dB.
    pub snr: i16,
    pub flags: CrossingFlags,
}

impl Crossing {
    /// Return a copy with every timestamp passed through `f` (e.g. a clock mapping).
    pub fn map_times(mut self, mut f: impl FnMut(u64) -> u64) -> Self {
        self.t_enter = f(self.t_enter);
        self.t_exit = f(self.t_exit);
        self.t_mid = f(self.t_mid);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CrossingFlags(pub u8);

impl CrossingFlags {
    /// Some sample in the crossing was flagged clipped by the Node.
    pub const CLIPPED: Self = Self(1 << 0);
    /// SNR below `Config::low_snr`.
    pub const LOW_SNR: Self = Self(1 << 1);
    /// Some sample in the crossing was flagged post-hop.
    pub const HOP_ARTIFACT: Self = Self(1 << 2);
    /// The crossing outlasted the sample buffer and was closed early.
    pub const TRUNCATED: Self = Self(1 << 3);

    pub const fn empty() -> Self {
        Self(0)
    }
    pub const fn contains(self, o: Self) -> bool {
        self.0 & o.0 == o.0
    }
    pub fn set(&mut self, o: Self) {
        self.0 |= o.0;
    }
}
