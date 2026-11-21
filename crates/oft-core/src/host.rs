//! The Timer side of one Node link: owns the clock mapping and a detector per seat, and
//! turns Node messages into crossings on the **Timer clock**.
//!
//! Hosts (oftd, the RP2350 firmware, Grid in-process) feed it decoded [`Message`]s and the
//! Timer time of their own SYNC pulses and pings. It has no I/O of its own.

use crate::clock::ClockMap;
use crate::detect::{Config, Detector, Threshold};
use crate::Crossing;
use oft_nodelink::msg::{Hello, Message, Plan, Samples, Status};

/// Seats one host can run per Node. Matches `Plan`'s capacity.
pub const MAX_SEATS: usize = oft_nodelink::msg::MAX_PLAN_SEATS;

/// Things a host should act on after feeding a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The Node (re)announced itself. Send it a `Plan`.
    Hello(Hello),
    /// A crossing finished, on the Timer clock.
    Crossing(Crossing),
    /// Periodic Node health.
    Status(Status),
    /// A pong matched an outstanding ping and was folded into the clock map.
    Synced,
}

/// One outstanding ping.
#[derive(Debug, Clone, Copy)]
struct Outstanding {
    id: u32,
    t_sent: u64,
}

pub struct NodeHost {
    clock: ClockMap,
    detectors: [Option<Detector>; MAX_SEATS],
    cfg: Config,
    /// Per-seat threshold overrides; `None` uses `cfg.threshold`.
    seat_thresholds: [Option<Threshold>; MAX_SEATS],
    hello: Option<Hello>,
    plan: Option<Plan>,
    pings: [Option<Outstanding>; 4],
    next_ping: u32,
    /// Timer time of the last SYNC pulse, by edge sequence, for pairing with `SyncEdge`.
    sync_pulses: [(u32, u64); 8],
    sync_pulse_next: usize,
    sync_seq: u32,
    /// SYNC pairs folded into the clock. Once > 0, pongs no longer touch the clock:
    /// a wire edge is far better than any software round trip.
    sync_pairs: u32,
    last_samples_t: Option<u64>,
    pub dropped_samples: u32,
}

impl NodeHost {
    pub fn new(cfg: Config) -> Self {
        Self {
            clock: ClockMap::new(),
            detectors: core::array::from_fn(|_| None),
            cfg,
            seat_thresholds: [None; MAX_SEATS],
            hello: None,
            plan: None,
            pings: [None; 4],
            next_ping: 1,
            sync_pulses: [(0, 0); 8],
            sync_pulse_next: 0,
            sync_seq: 0,
            sync_pairs: 0,
            last_samples_t: None,
            dropped_samples: 0,
        }
    }

    pub fn clock(&self) -> &ClockMap {
        &self.clock
    }

    /// Which sync source the clock map is built from.
    pub fn sync_source(&self) -> &'static str {
        if self.sync_pairs > 0 {
            "sync"
        } else if self.clock.is_synced() {
            "ping"
        } else {
            "none"
        }
    }

    pub fn hello(&self) -> Option<&Hello> {
        self.hello.as_ref()
    }

    pub fn plan(&self) -> Option<&Plan> {
        self.plan.as_ref()
    }

    pub fn detector(&self, seat: u8) -> Option<&Detector> {
        self.detectors.get(seat as usize).and_then(|d| d.as_ref())
    }

    pub fn detect_config(&self) -> &Config {
        &self.cfg
    }

    /// Detection settings in use for `seat`: the shared config with that seat's threshold
    /// override, if it has one.
    pub fn seat_detect_config(&self, seat: u8) -> Config {
        match self.seat_thresholds.get(seat as usize).copied().flatten() {
            Some(threshold) => Config { threshold, ..self.cfg },
            None => self.cfg,
        }
    }

    /// A seat's own threshold, if set.
    pub fn seat_threshold(&self, seat: u8) -> Option<Threshold> {
        self.seat_thresholds.get(seat as usize).copied().flatten()
    }

    /// Change the shared detection settings (takes effect immediately). Seats with their
    /// own threshold keep it.
    pub fn set_detect_config(&mut self, cfg: Config) {
        self.cfg = cfg;
        self.apply_detect_configs();
    }

    /// Give `seat` its own threshold, or `None` to go back to the shared one (takes effect
    /// immediately). Seats outside `MAX_SEATS` are ignored.
    pub fn set_seat_threshold(&mut self, seat: u8, threshold: Option<Threshold>) {
        if let Some(slot) = self.seat_thresholds.get_mut(seat as usize) {
            *slot = threshold;
            self.apply_detect_configs();
        }
    }

    fn apply_detect_configs(&mut self) {
        for seat in 0..MAX_SEATS {
            let cfg = self.seat_detect_config(seat as u8);
            if let Some(d) = self.detectors[seat].as_mut() {
                d.set_config(cfg);
            }
        }
    }

    /// Record the plan the host sent to the Node, and create detectors for its seats.
    pub fn set_plan(&mut self, plan: Plan) {
        for d in self.detectors.iter_mut() {
            *d = None;
        }
        for &(seat, _) in plan.seats() {
            let cfg = self.seat_detect_config(seat);
            if let Some(slot) = self.detectors.get_mut(seat as usize) {
                *slot = Some(Detector::new(seat, cfg));
            }
        }
        self.plan = Some(plan);
    }

    /// Build a `Ping` to send now. Remember it so the pong can be matched.
    pub fn make_ping(&mut self, t_timer: u64) -> Message {
        let id = self.next_ping;
        self.next_ping = self.next_ping.wrapping_add(1);
        let slot = (id as usize) % self.pings.len();
        self.pings[slot] = Some(Outstanding { id, t_sent: t_timer });
        Message::Ping(oft_nodelink::msg::Ping { id, t_timer })
    }

    /// The host pulsed SYNC at `t_timer`. Returns the edge sequence the Node will report.
    pub fn sync_pulsed(&mut self, t_timer: u64) -> u32 {
        self.sync_seq = self.sync_seq.wrapping_add(1);
        self.sync_pulses[self.sync_pulse_next] = (self.sync_seq, t_timer);
        self.sync_pulse_next = (self.sync_pulse_next + 1) % self.sync_pulses.len();
        self.sync_seq
    }

    /// Feed one decoded message from the Node. `t_timer` is when it arrived. Events are
    /// pushed to `out`; at most one crossing per sample, so a batch may yield several.
    pub fn feed(&mut self, msg: &Message, t_timer: u64, mut out: impl FnMut(Event)) {
        match msg {
            Message::Hello(h) => {
                self.hello = Some(*h);
                // A fresh Node clock: forget the old mapping.
                self.clock = ClockMap::new();
                self.sync_pairs = 0;
                self.last_samples_t = None;
                out(Event::Hello(*h));
            }
            Message::Samples(s) => self.feed_samples(s, &mut out),
            Message::SyncEdge(e) => {
                // Pair by time, not by sequence number: either side may restart its
                // counter. The edge belongs to the latest pulse issued before it arrived,
                // provided it arrived before the following pulse would have.
                if let Some(t_pulse) = self.pulse_for(t_timer) {
                    self.clock.add_pair(e.t_node, t_pulse);
                    self.sync_pairs += 1;
                    out(Event::Synced);
                }
            }
            Message::Pong(p) => {
                let slot = (p.id as usize) % self.pings.len();
                if let Some(o) = self.pings[slot] {
                    if o.id == p.id && o.t_sent == p.t_timer {
                        self.pings[slot] = None;
                        if self.sync_pairs == 0 && self.clock.add_ping(o.t_sent, t_timer, p.t_node) {
                            out(Event::Synced);
                        }
                    }
                }
            }
            Message::Status(s) => out(Event::Status(*s)),
            // Timer → Node messages echoed back, or Ping: nothing to do.
            _ => {}
        }
    }

    /// The SYNC pulse an edge arriving at `t_arrival` belongs to.
    fn pulse_for(&self, t_arrival: u64) -> Option<u64> {
        let mut latest: Option<u64> = None;
        let mut previous: Option<u64> = None;
        for &(seq, t) in &self.sync_pulses {
            if seq == 0 || t > t_arrival {
                continue;
            }
            match latest {
                Some(l) if t <= l => {
                    if previous.map_or(true, |p| t > p) {
                        previous = Some(t);
                    }
                }
                _ => {
                    previous = latest;
                    latest = Some(t);
                }
            }
        }
        let l = latest?;
        match previous {
            // Must have arrived within one pulse period of its pulse.
            Some(p) => (t_arrival - l < l - p).then_some(l),
            None => Some(l),
        }
    }

    fn feed_samples(&mut self, s: &Samples, out: &mut impl FnMut(Event)) {
        let Some(det) = self.detectors.get_mut(s.seat as usize).and_then(|d| d.as_mut()) else {
            self.dropped_samples = self.dropped_samples.wrapping_add(s.len() as u32);
            return;
        };
        if let Some(last) = self.last_samples_t {
            // Gap detection: more than two sample periods missing.
            if s.t0 > last + 3 * s.dt_us as u64 {
                self.dropped_samples = self.dropped_samples.wrapping_add(((s.t0 - last) / s.dt_us.max(1) as u64) as u32);
            }
        }
        // A Node with no floor estimate for this batch sends an impossible value; ignore it.
        if s.noise_floor > -3000 {
            det.floor_hint(s.noise_floor);
        }
        for (k, &p) in s.power().iter().enumerate() {
            let t = s.time_of(k);
            if let Some(c) = det.push(t, p, s.flags) {
                let clock = &self.clock;
                out(Event::Crossing(c.map_times(|t| clock.to_timer(t))));
            }
        }
        self.last_samples_t = Some(s.time_of(s.len().saturating_sub(1)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oft_nodelink::msg::{Features, Hello, Pong, SyncEdge};

    #[test]
    fn a_seat_threshold_overrides_the_shared_one_and_survives_replanning() {
        let own = Threshold::Manual { enter: -700, exit: -760 };
        let shared = Threshold::Manual { enter: -500, exit: -550 };
        let mut plan = Plan::with(0, 0, oft_nodelink::msg::GAIN_AUTO);
        plan.push(0, 5658).unwrap();
        plan.push(1, 5732).unwrap();

        let mut host = NodeHost::new(Config::default());
        host.set_plan(plan);
        host.set_seat_threshold(1, Some(own));
        assert_eq!(host.detector(0).unwrap().config().threshold, Config::default().threshold);
        assert_eq!(host.detector(1).unwrap().levels(), (-700, -760));

        // The shared config changes under it; seat 1 keeps its own, and min-lap still applies.
        host.set_detect_config(Config { threshold: shared, min_lap_us: 3_000_000, ..Config::default() });
        assert_eq!(host.detector(0).unwrap().levels(), (-500, -550));
        assert_eq!(host.detector(1).unwrap().levels(), (-700, -760));
        assert_eq!(host.detector(1).unwrap().config().min_lap_us, 3_000_000);

        // New detectors from a re-plan pick the override up too.
        host.set_plan(plan);
        assert_eq!(host.detector(1).unwrap().levels(), (-700, -760));

        // Clearing it falls back to the shared one. Out-of-range seats are ignored.
        host.set_seat_threshold(1, None);
        host.set_seat_threshold(200, Some(own));
        assert_eq!(host.detector(1).unwrap().levels(), (-500, -550));
        assert_eq!(host.seat_threshold(1), None);
    }

    #[test]
    fn pongs_are_ignored_once_a_sync_wire_is_seen() {
        let mut host = NodeHost::new(Config::default());
        // Node clock = timer clock + 1 s, no drift.
        for k in 1..=20u64 {
            let t = k * 50_000;
            let seq = host.sync_pulsed(t);
            host.feed(&Message::SyncEdge(SyncEdge { edge_seq: seq, t_node: t + 1_000_000 }), t + 10, |_| {});
        }
        assert_eq!(host.sync_source(), "sync");
        let before = host.clock().residual_us();
        // A ping whose pong arrives 8 ms late (quantised read loop) would skew the fit by ms.
        let ping = host.make_ping(1_100_000);
        let Message::Ping(p) = ping else { unreachable!() };
        host.feed(&Message::Pong(Pong { id: p.id, t_timer: p.t_timer, t_node: 1_100_000 + 1_000_000 + 150 }), 1_108_000, |_| {});
        assert_eq!(host.clock().residual_us(), before);
        assert_eq!(host.clock().len(), 20);
    }

    #[test]
    fn sync_edges_pair_by_time_even_with_mismatched_sequence_numbers() {
        let mut host = NodeHost::new(Config::default());
        // Node clock = timer + 1 s. Node's edge counter starts at 7, host's at 1.
        for k in 1..=10u64 {
            let t = k * 50_000;
            host.sync_pulsed(t);
            host.feed(&Message::SyncEdge(SyncEdge { edge_seq: 6 + k as u32, t_node: t + 1_000_000 }), t + 300, |_| {});
        }
        assert_eq!(host.clock().len(), 10);
        assert!(host.clock().residual_us() < 1.0);
        // An edge that shows up a whole period late is not paired with the wrong pulse.
        host.sync_pulsed(550_000);
        host.feed(&Message::SyncEdge(SyncEdge { edge_seq: 99, t_node: 1_550_000 }), 550_000 + 60_000, |_| {});
        assert_eq!(host.clock().len(), 10);
        // A Hello (Node reboot) clears the clock; the next edges resync without any seq agreement.
        host.feed(&Message::Hello(Hello { chip_id: 1, fw: [0, 1, 0], link_version: 1, freq_min_mhz: 5645, freq_max_mhz: 5945, max_seats: 8, features: Features::SYNC, chip: Default::default() }), 600_000, |_| {});
        assert!(!host.clock().is_synced());
        for k in 13..=16u64 {
            let t = k * 50_000;
            host.sync_pulsed(t);
            host.feed(&Message::SyncEdge(SyncEdge { edge_seq: k as u32, t_node: t + 2_000_000 }), t + 300, |_| {});
        }
        assert!(host.clock().is_synced());
        assert_eq!(host.clock().to_timer(2_800_000 + 2_000_000), 2_800_000);
    }

    #[test]
    fn an_impossible_noise_floor_hint_is_ignored() {
        use oft_nodelink::msg::{Plan, SampleFlags, Samples, GAIN_AUTO};
        let mut host = NodeHost::new(Config::default());
        let mut plan = Plan::with(0, 0, GAIN_AUTO);
        plan.push(0, 5800).unwrap();
        host.set_plan(plan);
        let mut s = Samples::new(0, 0, 1000, i16::MIN, SampleFlags::empty());
        for _ in 0..10 {
            s.push(-915).unwrap();
        }
        host.feed(&Message::Samples(s), 0, |_| {});
        let floor = host.detector(0).unwrap().floor().unwrap();
        assert!(floor > -1000, "floor {floor} was poisoned by the sentinel");
    }

    #[test]
    fn pings_do_sync_when_there_is_no_wire() {
        let mut host = NodeHost::new(Config::default());
        let mut synced = 0;
        for k in 1..=10u64 {
            let t = k * 200_000;
            let Message::Ping(p) = host.make_ping(t) else { unreachable!() };
            host.feed(&Message::Pong(Pong { id: p.id, t_timer: p.t_timer, t_node: t + 500 + 1_000_000 }), t + 1000, |e| if e == Event::Synced { synced += 1 });
        }
        assert_eq!(synced, 10);
        assert_eq!(host.sync_source(), "ping");
        assert!((host.clock().to_timer(5_000_000) as i64 - 4_000_000).abs() < 50);
    }

    #[test]
    fn a_pong_with_the_wrong_sent_time_is_rejected() {
        let mut host = NodeHost::new(Config::default());
        let Message::Ping(p) = host.make_ping(1000) else { unreachable!() };
        let mut synced = 0;
        host.feed(&Message::Pong(Pong { id: p.id, t_timer: 999, t_node: 5 }), 2000, |e| if e == Event::Synced { synced += 1 });
        assert_eq!(synced, 0);
        assert!(!host.clock().is_synced());
    }
}
