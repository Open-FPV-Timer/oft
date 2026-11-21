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
    /// This seat's own threshold. Absent or null: use `Config::threshold`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<ThresholdConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ThresholdConfig {
    /// 0.1 dB above the tracked floor; exit = enter − hysteresis.
    Auto { snr_enter: i16, hysteresis: i16 },
    /// Absolute 0.1 dB levels.
    Manual { enter: i16, exit: i16 },
}

impl From<&ThresholdConfig> for oft_core::Threshold {
    fn from(t: &ThresholdConfig) -> Self {
        match *t {
            ThresholdConfig::Auto { snr_enter, hysteresis } => Self::Auto { snr_enter, hysteresis },
            ThresholdConfig::Manual { enter, exit } => Self::Manual { enter, exit },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub seats: Vec<SeatConfig>,
    /// Timer-wide default; a seat's own `threshold` overrides it for that seat.
    pub threshold: ThresholdConfig,
    pub min_lap_ms: u32,
    /// Exit hold-off: how long the signal must stay below the exit level before a crossing
    /// ends. Absent (older clients) means the default.
    #[serde(default = "default_exit_hold_ms")]
    pub exit_hold_ms: u32,
    /// Pass window: crossings that start within this long of the first one are one pass,
    /// and only the strongest is reported, once the window has closed. 0 (and absent) = off.
    #[serde(default)]
    pub pass_window_ms: u32,
}

pub fn default_exit_hold_ms() -> u32 {
    (oft_core::DetectConfig::default().exit_hold_us / 1000) as u32
}

impl Config {
    /// Why this config cannot be used, if it cannot.
    pub fn check(&self) -> Result<(), &'static str> {
        if self.seats.iter().any(|s| s.seat >= 8) {
            return Err("seat must be 0..7");
        }
        if self.seats.iter().any(|s| !(5000..=6200).contains(&s.freq_mhz)) {
            return Err("freq_mhz out of range");
        }
        if self.exit_hold_ms > 1000 {
            return Err("exit_hold_ms must be 0..1000");
        }
        if self.pass_window_ms > 10_000 {
            return Err("pass_window_ms must be 0..10000");
        }
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            seats: vec![SeatConfig { seat: 0, freq_mhz: 5800, enabled: true, threshold: None }],
            threshold: ThresholdConfig::Auto { snr_enter: 150, hysteresis: 50 },
            min_lap_ms: 2000,
            exit_hold_ms: default_exit_hold_ms(),
            pass_window_ms: 0,
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

/// Health of the Node link, so lost samples can be traced to their cause. Counts since the
/// Timer started; `null` where this host cannot tell.
#[derive(Debug, Clone, Serialize)]
pub struct LinkJson {
    /// Samples the Node's timestamps show were never received.
    pub gap_samples: u32,
    /// Messages that arrived damaged (bad checksum or framing).
    pub crc_errors: u32,
    /// Receive errors reported by the serial port (overrun, framing, noise).
    pub uart_errors: Option<u32>,
    /// Messages the Node says it dropped before sending.
    pub node_overruns: Option<u16>,
    pub node_hop_errors: Option<u16>,
    /// Gain index in use on the Node (255 = automatic).
    pub node_gain: Option<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusJson {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub uptime_s: u64,
    pub time_us: u64,
    /// The Node's chip temperature.
    pub temp_c: Option<f32>,
    /// The Timer host's own temperature.
    pub timer_temp_c: Option<f32>,
    pub dropped: u32,
    pub clock_err_us: Option<f64>,
    pub clock_synced: bool,
    pub clock_source: &'static str,
    pub drift_ppm: Option<f64>,
    pub node_connected: bool,
    pub link: LinkJson,
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
