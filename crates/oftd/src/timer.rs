//! The Timer: one Node (for now), its host state, crossings, sample history, and the
//! driver loop that moves bytes between the link and the host.

use crate::link::NodeLink;
use crate::protocol::{self, Config, CrossingJson, LinkJson, SamplesJson, SeatStatus, StatusJson};
use oft_core::{Crossing, DetectConfig, Event, NodeHost, MAX_SEATS};
use oft_nodelink::msg::{Message, Plan, Samples, GAIN_AUTO};
use oft_nodelink::{encode_frame, FrameDecoder, MAX_FRAME};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// Crossings kept for `/crossings?since=` and reconnect replay.
pub const CROSSING_REPLAY: usize = 256;
/// Seconds of full-rate sample history kept per seat.
pub const HISTORY_S: u64 = 600;
/// Conformance level (spec §6.2).
pub const LEVEL: &str = "L3";

#[derive(Debug, Clone)]
pub struct TimerConfig {
    pub id: String,
    pub name: String,
    pub sync_hz: u32,
    pub ping_period: Duration,
    pub tick: Duration,
    pub status_period: Duration,
    /// The settings file (`settings.rs`). `None`: nothing is kept across a restart.
    pub settings: Option<std::path::PathBuf>,
    /// A Node this Timer can restart into its bootloader and update (`nodefw.rs`).
    pub node_fw: Option<crate::nodefw::NodeFlasher>,
}

/// `oft-` and four hex digits that stay the same on this machine (from `/etc/machine-id`),
/// since clients key on a Timer's id. Without a machine id, the process id: unique, not stable.
fn default_id() -> String {
    let n = match std::fs::read_to_string("/etc/machine-id") {
        // FNV-1a over the id, folded to 16 bits.
        Ok(id) if !id.trim().is_empty() => {
            let h = id.trim().bytes().fold(0x811c_9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193));
            (h ^ (h >> 16)) & 0xFFFF
        }
        _ => std::process::id() & 0xFFFF,
    };
    format!("oft-{n:04x}")
}

impl Default for TimerConfig {
    fn default() -> Self {
        Self {
            id: default_id(),
            name: "OFT Timer".into(),
            sync_hz: 20,
            ping_period: Duration::from_secs(1),
            tick: Duration::from_millis(20),
            status_period: Duration::from_secs(1),
            settings: None,
            node_fw: None,
        }
    }
}

/// Anything pushed to WebSocket clients.
#[derive(Debug, Clone)]
pub enum Outgoing {
    Crossing(CrossingJson),
    Samples(SamplesJson),
    Status(StatusJson),
    /// A claim or the holder of control changed (spec §6.6).
    Control,
}

/// Standing control claims a Timer keeps.
pub const MAX_CLAIMS: usize = 16;

struct SeatHistory {
    /// (timer time µs, power 0.1 dB), oldest first.
    samples: VecDeque<(u64, i16)>,
    last_rssi: Option<i16>,
    rate_hz: u32,
}

pub struct Inner {
    pub host: NodeHost,
    pub config: Config,
    /// What the settings file holds (kept in step by `save_config`).
    pub saved: Option<Config>,
    pub crossings: VecDeque<(u64, Crossing)>,
    pub next_seq: u64,
    history: Vec<Option<SeatHistory>>,
    node_connected: bool,
    node_temp_dc: Option<i16>,
    node_status: Option<oft_nodelink::msg::Status>,
    crc_errors: u32,
    last_node_rx: Option<Instant>,
}

pub struct Timer {
    pub cfg: TimerConfig,
    pub boot_id: String,
    started: Instant,
    pub inner: Mutex<Inner>,
    pub(crate) link: Mutex<Box<dyn NodeLink>>,
    /// A Node firmware update has the link: the driver loop leaves it alone.
    pub(crate) flashing: AtomicBool,
    /// Times a Node has introduced itself.
    pub(crate) hellos: AtomicU32,
    pub node_fw: Mutex<crate::nodefw::NodeFw>,
    pub events: broadcast::Sender<Outgoing>,
    pub clients: AtomicUsize,
    /// Who may write (spec §6.6).
    pub control: Mutex<oft_core::control::Control<MAX_CLAIMS>>,
    /// `config_rev` in `/status`.
    config_rev: AtomicU32,
}

fn detect_config(c: &Config) -> DetectConfig {
    DetectConfig {
        threshold: (&c.threshold).into(),
        min_lap_us: c.min_lap_ms as u64 * 1000,
        exit_hold_us: c.exit_hold_ms as u64 * 1000,
        pass_window_us: c.pass_window_ms as u64 * 1000,
        ..DetectConfig::default()
    }
}

/// Push the Timer-wide detection settings and every seat's own threshold into the host.
/// Call before `set_plan`, which builds the detectors from them.
fn apply_detection(host: &mut NodeHost, c: &Config) {
    host.set_detect_config(detect_config(c));
    for seat in 0..MAX_SEATS as u8 {
        let own = c.seats.iter().find(|s| s.seat == seat).and_then(|s| s.threshold.as_ref());
        host.set_seat_threshold(seat, own.map(Into::into));
    }
}

/// This machine's temperature where Linux reports one (a Raspberry Pi does), else `None`.
fn host_temp_c() -> Option<f32> {
    let milli: f32 = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").ok()?.trim().parse().ok()?;
    Some(milli / 1000.0)
}

fn plan_for(c: &Config) -> Plan {
    let mut p = Plan::with(0, 0, GAIN_AUTO);
    for s in c.seats.iter().filter(|s| s.enabled) {
        let _ = p.push(s.seat, s.freq_mhz);
    }
    p
}

impl Timer {
    /// `config` is what the Timer starts with unless its settings file holds a saved one.
    pub fn new(cfg: TimerConfig, link: Box<dyn NodeLink>, config: Config) -> Arc<Self> {
        let saved = cfg.settings.as_deref().and_then(crate::settings::load);
        let config = saved.clone().unwrap_or(config);
        let (events, _) = broadcast::channel(1024);
        let boot_id = format!("{:08x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0));
        let mut host = NodeHost::new(detect_config(&config));
        apply_detection(&mut host, &config);
        host.set_plan(plan_for(&config));
        Arc::new(Self {
            cfg,
            started: Instant::now(),
            inner: Mutex::new(Inner {
                host,
                config,
                saved,
                crossings: VecDeque::new(),
                next_seq: 1,
                history: (0..8).map(|_| None).collect(),
                node_connected: false,
                node_temp_dc: None,
                node_status: None,
                crc_errors: 0,
                last_node_rx: None,
            }),
            link: Mutex::new(link),
            flashing: AtomicBool::new(false),
            hellos: AtomicU32::new(0),
            node_fw: Mutex::new(Default::default()),
            events,
            clients: AtomicUsize::new(0),
            control: Mutex::new(oft_core::control::Control::new(u32::from_str_radix(&boot_id, 16).unwrap_or(1))),
            config_rev: AtomicU32::new(0),
            boot_id,
        })
    }

    /// Write the power-on config to the settings file (`None` forgets it) and, once it is
    /// safely there, record it as the saved state.
    pub fn save_config(&self, config: Option<Config>) -> Result<(), String> {
        let path = self.cfg.settings.as_deref().ok_or("this Timer does not keep settings")?;
        crate::settings::save(path, config.as_ref()).map_err(|e| format!("could not write {}: {e}", path.display()))?;
        self.inner.lock().unwrap().saved = config;
        self.config_rev.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Timer clock: µs since start.
    pub fn now_us(&self) -> u64 {
        self.started.elapsed().as_micros() as u64
    }

    pub fn uptime_s(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    fn send_to_node(&self, m: &Message) {
        let mut buf = [0u8; MAX_FRAME];
        if let Ok(n) = encode_frame(m, &mut buf) {
            let t = self.now_us();
            self.link.lock().unwrap().write(&buf[..n], t);
        }
    }

    /// Apply a new `/config`. Detection settings take effect at once. Only a change of seats
    /// or frequencies re-plans the Node: that restarts its sample stream and resets the
    /// detectors, so a change of levels alone must not do it.
    pub fn set_config(&self, config: Config) {
        let plan = plan_for(&config);
        let replan = {
            let mut inner = self.inner.lock().unwrap();
            apply_detection(&mut inner.host, &config);
            let replan = inner.host.plan() != Some(&plan);
            if replan {
                inner.host.set_plan(plan);
                for h in inner.history.iter_mut() {
                    *h = None;
                }
            }
            inner.config = config;
            replan
        };
        self.config_rev.fetch_add(1, Ordering::Relaxed);
        if replan {
            self.send_to_node(&Message::Plan(plan));
        }
    }

    /// Crossings after `since` (exclusive), oldest first.
    pub fn crossings_since(&self, since: u64) -> Vec<CrossingJson> {
        self.inner.lock().unwrap().crossings.iter().filter(|(s, _)| *s > since).map(|(s, c)| CrossingJson::new(*s, c)).collect()
    }

    /// Sample history for one seat within `[from, to]` on the Timer clock.
    pub fn samples(&self, seat: u8, from: u64, to: u64) -> Option<SamplesJson> {
        let inner = self.inner.lock().unwrap();
        let h = inner.history.get(seat as usize)?.as_ref()?;
        let pts: Vec<(u64, i16)> = h.samples.iter().copied().filter(|(t, _)| *t >= from && *t <= to).collect();
        let (t0, _) = *pts.first()?;
        let dt_us = match 1_000_000u32.checked_div(h.rate_hz) {
            Some(d) => d,
            None if pts.len() > 1 => ((pts.last()?.0 - t0) / (pts.len() as u64 - 1)) as u32,
            None => 1000,
        };
        let floor = inner.host.detector(seat).and_then(|d| d.floor()).unwrap_or(0);
        Some(SamplesJson { kind: "samples", seat, t0, dt_us, p: pts.iter().map(|(_, p)| *p).collect(), nf: floor })
    }

    pub fn status(&self) -> StatusJson {
        let inner = self.inner.lock().unwrap();
        let clock = inner.host.clock();
        let seats = inner
            .config
            .seats
            .iter()
            .filter(|s| s.enabled)
            .map(|s| {
                let det = inner.host.detector(s.seat);
                let (enter, exit) = det.map(|d| d.levels()).unwrap_or((0, 0));
                SeatStatus {
                    seat: s.seat,
                    node: inner.host.hello().map(|h| format!("{:012x}", h.chip_id)),
                    rate_hz: inner.history.get(s.seat as usize).and_then(|h| h.as_ref()).map(|h| h.rate_hz).unwrap_or(0),
                    rssi: inner.history.get(s.seat as usize).and_then(|h| h.as_ref()).and_then(|h| h.last_rssi),
                    floor: det.and_then(|d| d.floor()),
                    enter,
                    exit,
                    crossing: det.is_some_and(|d| d.is_crossing()),
                }
            })
            .collect();
        StatusJson {
            kind: "status",
            uptime_s: self.uptime_s(),
            time_us: self.now_us(),
            temp_c: inner.node_temp_dc.map(|t| t as f32 / 10.0),
            timer_temp_c: host_temp_c(),
            dropped: inner.host.dropped_samples,
            clock_err_us: clock.is_synced().then(|| clock.residual_us()),
            clock_synced: clock.is_synced(),
            clock_source: inner.host.sync_source(),
            drift_ppm: clock.is_synced().then(|| clock.drift_ppm()),
            node_connected: inner.node_connected,
            problems: if inner.node_connected { vec![] } else { vec!["node_missing"] },
            link: LinkJson {
                gap_samples: inner.host.dropped_samples,
                crc_errors: inner.crc_errors,
                uart_errors: None,
                node_overruns: inner.node_status.map(|s| s.overruns),
                node_hop_errors: inner.node_status.map(|s| s.hop_errors),
                node_gain: inner.node_status.map(|s| s.gain_index),
            },
            seats,
            clients: self.clients.load(Ordering::Relaxed),
            config_rev: self.config_rev.load(Ordering::Relaxed),
            control: self.holder(),
        }
    }

    /// The claim that holds control.
    pub fn holder(&self) -> Option<protocol::HolderJson> {
        self.control.lock().unwrap().holder().map(|c| protocol::HolderJson { id: c.id, name: c.name.as_str().into() })
    }

    /// The reply of `/control`: the standing claims, oldest first.
    pub fn control_json(&self) -> serde_json::Value {
        let c = self.control.lock().unwrap();
        let holder = c.holder().map(|h| h.id);
        serde_json::json!({
            "holder": holder,
            "claims": c.claims().map(|x| serde_json::json!({ "id": x.id, "name": x.name.as_str(), "holds": Some(x.id) == holder, "connected": x.away_until.is_none() })).collect::<Vec<_>>(),
        })
    }

    pub fn info(&self) -> serde_json::Value {
        let inner = self.inner.lock().unwrap();
        let hello = inner.host.hello();
        let flashing = self.flashing.load(Ordering::Relaxed);
        // An update holds the link for its whole run: do not wait for it here.
        let (has_sync, link_name) = match self.link.try_lock() {
            Ok(l) => (l.has_sync(), l.describe()),
            Err(_) => (false, "busy".into()),
        };
        let mut v = serde_json::json!({
            "id": self.cfg.id,
            "name": self.cfg.name,
            "boot_id": self.boot_id,
            "proto": protocol::PROTO,
            "host": "oftd",
            "fw": env!("CARGO_PKG_VERSION"),
            "level": LEVEL,
            "calibrated": false,
            "nodes": hello.map(|h| vec![serde_json::json!({
                "id": format!("{:012x}", h.chip_id),
                "chip": h.chip_name(),
                "fw": format!("{}.{}.{}", h.fw[0], h.fw[1], h.fw[2]),
                "range_mhz": [h.freq_min_mhz, h.freq_max_mhz],
                "max_seats": h.max_seats,
                "level": { "unit": h.level().unit(), "scale": h.level().scale().parse::<f64>().unwrap_or(0.1) },
                "link": link_name,
            })]).filter(|_| !flashing).unwrap_or_default(),
            "seats": inner.config.seats.iter().filter(|s| s.enabled).map(|s| serde_json::json!({
                "seat": s.seat, "node": 0,
                "rate_hz": inner.history.get(s.seat as usize).and_then(|h| h.as_ref()).map(|h| h.rate_hz).unwrap_or(0),
            })).collect::<Vec<_>>(),
            "capabilities": {
                "crossings": { "algorithms": ["edge_mid"], "replay": CROSSING_REPLAY, "fields": ["t_enter", "t_exit", "peak", "snr"] },
                "samples.stream": { "rate_hz": 1000, "decimate": [1, 2, 5, 10, 20, 50] },
                "samples.history": { "full_rate_s": HISTORY_S, "reduced_rate_s": 0, "reduced_hz": 0 },
                "config": { "persist": self.cfg.settings.is_some(), "threshold_modes": ["auto", "manual"], "min_lap_ms": true, "seat_thresholds": true, "exit_hold_ms": true, "pass_window_ms": true },
                "time": { "sync": if has_sync { vec!["sync", "ping"] } else { vec!["ping"] } },
                "control": {},
            },
            // The Timer's own page, and what that page can do here. Not part of the OFT
            // Protocol beyond `manage_url` (spec 6.3): race software only links to it.
            "manage_url": "/",
            "manage": {},
        });
        match self.cfg.node_fw {
            Some(crate::nodefw::NodeFlasher::Stm32(_)) => v["manage"]["fw.update"] = serde_json::json!({ "nodes": ["uart"], "self": false, "node_image": "stm32f1" }),
            Some(crate::nodefw::NodeFlasher::Esp(_)) => v["manage"]["fw.update"] = serde_json::json!({ "nodes": ["uart"], "self": false }),
            None => {}
        }
        v
    }

    /// TXT record for mDNS discovery (spec §6.1), in a fixed key order.
    pub fn discovery_txt(&self) -> Vec<(&'static str, String)> {
        let inner = self.inner.lock().unwrap();
        vec![
            ("id", self.cfg.id.clone()),
            ("fw", env!("CARGO_PKG_VERSION").to_string()),
            ("proto", protocol::PROTO.to_string()),
            ("seats", inner.config.seats.iter().filter(|s| s.enabled).count().to_string()),
            ("nodes", (inner.host.hello().is_some() as u8).to_string()),
            ("level", LEVEL.to_string()),
        ]
    }

    pub fn hello_json(&self) -> serde_json::Value {
        let mut v = self.info();
        let obj = v.as_object_mut().unwrap();
        obj.insert("type".into(), "hello".into());
        obj.insert("timer".into(), self.cfg.id.clone().into());
        obj.insert("time_us".into(), self.now_us().into());
        obj.insert("encodings".into(), serde_json::json!(["json"]));
        v
    }

    /// Run the Node driver until the task is dropped. Call once, in its own task.
    pub async fn run(self: Arc<Self>) {
        let mut tick = tokio::time::interval(self.cfg.tick);
        let sync_period = Duration::from_micros(1_000_000 / self.cfg.sync_hz.max(1) as u64);
        let mut next_sync = Instant::now();
        let mut next_ping = Instant::now();
        let mut next_status = Instant::now() + self.cfg.status_period;
        let mut decoder = FrameDecoder::new();
        loop {
            tick.tick().await;
            if self.flashing.load(Ordering::SeqCst) {
                // A firmware update has the link (nodefw.rs).
                decoder = FrameDecoder::new();
                self.inner.lock().unwrap().node_connected = false;
                continue;
            }
            let now = Instant::now();
            let t = self.now_us();

            if self.link.lock().unwrap().has_sync() && now >= next_sync {
                self.inner.lock().unwrap().host.sync_pulsed(t);
                self.link.lock().unwrap().sync_pulse(t);
                next_sync = now + sync_period;
            }
            if now >= next_ping {
                let ping = self.inner.lock().unwrap().host.make_ping(t);
                self.send_to_node(&ping);
                next_ping = now + self.cfg.ping_period;
            }

            let bytes = self.link.lock().unwrap().read(t);
            let mut replan = false;
            if !bytes.is_empty() {
                let mut inner = self.inner.lock().unwrap();
                inner.last_node_rx = Some(now);
                inner.node_connected = true;
                for b in bytes {
                    let m = match decoder.push(b) {
                        Some(Ok(m)) => Some(m),
                        Some(Err(_)) => {
                            inner.crc_errors = inner.crc_errors.wrapping_add(1);
                            None
                        }
                        None => None,
                    };
                    if let Some(m) = m {
                        if let Message::Samples(s) = &m {
                            self.record_samples(&mut inner, s, t);
                        }
                        let Inner { host, crossings, next_seq, node_temp_dc, node_status, .. } = &mut *inner;
                        host.feed(&m, t, |e| match e {
                            Event::Hello(_) => {
                                self.hellos.fetch_add(1, Ordering::Relaxed);
                                replan = true;
                            }
                            Event::Crossing(c) => {
                                let seq = *next_seq;
                                *next_seq += 1;
                                crossings.push_back((seq, c));
                                while crossings.len() > CROSSING_REPLAY {
                                    crossings.pop_front();
                                }
                                let _ = self.events.send(Outgoing::Crossing(CrossingJson::new(seq, &c)));
                            }
                            Event::Status(s) => {
                                // i16::MIN = the Node has no reading; 0 = Node firmware before it had a sensor.
                                *node_temp_dc = (s.temp_dc != i16::MIN && s.temp_dc != 0).then_some(s.temp_dc);
                                *node_status = Some(s);
                            }
                            Event::Synced => {}
                        });
                    }
                }
            } else {
                // One lock, released before the next statement: an `if let` on the lock
                // itself would still hold it inside its block and deadlock on a second lock.
                let mut inner = self.inner.lock().unwrap();
                if inner.last_node_rx.is_some_and(|last| now.duration_since(last) > Duration::from_secs(3)) {
                    inner.node_connected = false;
                }
            }
            if replan {
                let plan = plan_for(&self.inner.lock().unwrap().config);
                self.send_to_node(&Message::Plan(plan));
            }
            if now >= next_status {
                if self.control.lock().unwrap().expire(self.now_us()) {
                    let _ = self.events.send(Outgoing::Control);
                }
                let _ = self.events.send(Outgoing::Status(self.status()));
                next_status = now + self.cfg.status_period;
            }
        }
    }

    fn record_samples(&self, inner: &mut Inner, s: &Samples, _t_timer: u64) {
        let clock = *inner.host.clock();
        let slot = match inner.history.get_mut(s.seat as usize) {
            Some(slot) => slot,
            None => return,
        };
        let h = slot.get_or_insert_with(|| SeatHistory { samples: VecDeque::new(), last_rssi: None, rate_hz: 0 });
        if s.dt_us > 0 {
            h.rate_hz = 1_000_000 / s.dt_us as u32;
        }
        let t0 = clock.to_timer(s.t0);
        let dt = s.dt_us as u64;
        for (k, &p) in s.power().iter().enumerate() {
            h.samples.push_back((t0 + k as u64 * dt, p));
        }
        h.last_rssi = s.power().last().copied();
        let cutoff = t0.saturating_sub(HISTORY_S * 1_000_000);
        while h.samples.front().is_some_and(|(t, _)| *t < cutoff) {
            h.samples.pop_front();
        }
        let _ = self.events.send(Outgoing::Samples(SamplesJson {
            kind: "samples",
            seat: s.seat,
            t0,
            dt_us: s.dt_us as u32,
            p: s.power().to_vec(),
            nf: inner.host.detector(s.seat).and_then(|d| d.floor()).unwrap_or(s.noise_floor),
        }));
    }
}
