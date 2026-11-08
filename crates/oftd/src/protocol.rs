//! OFT Protocol JSON shapes (spec §6). Shared by the HTTP handlers and the WebSocket.

use oft_core::{Crossing, CrossingFlags};
use serde::{Deserialize, Serialize};

pub const PROTO: u32 = 1;
pub const API_PREFIX: &str = "/oft/v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SeatConfig {
    pub seat: u8,
    pub freq_mhz: u16,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ThresholdConfig {
    /// 0.1 dB above the tracked floor; exit = enter − hysteresis.
    Auto { snr_enter: i16, hysteresis: i16 },
    /// Absolute 0.1 dB levels.
    Manual { enter: i16, exit: i16 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub seats: Vec<SeatConfig>,
    pub seats_per_node: u8,
    pub threshold: ThresholdConfig,
    pub min_lap_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            seats: vec![SeatConfig { seat: 0, freq_mhz: 5800, enabled: true }],
            seats_per_node: 8,
            threshold: ThresholdConfig::Auto { snr_enter: 150, hysteresis: 50 },
            min_lap_ms: 2000,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CrossingJson {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub seq: u64,
    pub seat: u8,
    pub t_enter: u64,
    pub t_exit: u64,
    pub t_mid: u64,
    pub peak: i16,
    pub snr: i16,
    pub flags: Vec<&'static str>,
}

pub fn flag_names(f: CrossingFlags) -> Vec<&'static str> {
    let mut v = Vec::new();
    if f.contains(CrossingFlags::CLIPPED) {
        v.push("clipped");
    }
    if f.contains(CrossingFlags::LOW_SNR) {
        v.push("low_snr");
    }
    if f.contains(CrossingFlags::HOP_ARTIFACT) {
        v.push("hop_artifact");
    }
    if f.contains(CrossingFlags::TRUNCATED) {
        v.push("truncated");
    }
    v
}

impl CrossingJson {
    pub fn new(seq: u64, c: &Crossing) -> Self {
        Self {
            kind: "crossing",
            seq,
            seat: c.seat,
            t_enter: c.t_enter,
            t_exit: c.t_exit,
            t_mid: c.t_mid,
            peak: c.peak,
            snr: c.snr,
            flags: flag_names(c.flags),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SamplesJson {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub seat: u8,
    pub t0: u64,
    pub dt_us: u32,
    pub p: Vec<i16>,
    pub nf: i16,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeatStatus {
    pub seat: u8,
    pub node: Option<String>,
    pub rate_hz: u32,
    pub rssi: Option<i16>,
    pub floor: Option<i16>,
    pub enter: i16,
    pub exit: i16,
    pub crossing: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusJson {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub uptime_s: u64,
    pub time_us: u64,
    pub temp_c: Option<f32>,
    pub dropped: u32,
    pub clock_err_us: Option<f64>,
    pub clock_synced: bool,
    pub clock_source: &'static str,
    pub drift_ppm: Option<f64>,
    pub node_connected: bool,
    pub seats: Vec<SeatStatus>,
    pub clients: usize,
}

/// Client → Timer on the WebSocket.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Sub {
    pub crossings: Option<SubCrossings>,
    pub samples: Option<SubSamples>,
    #[serde(default)]
    pub status: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SubCrossings {
    pub since: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubSamples {
    #[serde(default = "all_seats")]
    pub seats: SeatSel,
    #[serde(default = "one")]
    pub decimate: u32,
}

fn all_seats() -> SeatSel {
    SeatSel::All("all".into())
}
fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SeatSel {
    All(String),
    List(Vec<u8>),
}

impl SeatSel {
    pub fn includes(&self, seat: u8) -> bool {
        match self {
            SeatSel::All(_) => true,
            SeatSel::List(v) => v.contains(&seat),
        }
    }
}
