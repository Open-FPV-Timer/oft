//! The Timer side of one Node link: owns the clock mapping and a detector per seat, and
//! turns Node messages into crossings on the **Timer clock**.
//!
//! Hosts (oftd, the RP2350 firmware, Grid in-process) feed it decoded [`Message`]s and the
//! Timer time of their own SYNC pulses and pings. It has no I/O of its own.

use crate::clock::ClockMap;
use crate::detect::{Config, Detector};
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
    hello: Option<Hello>,
    plan: Option<Plan>,
    pings: [Option<Outstanding>; 4],
    next_ping: u32,
    /// Timer time of the last SYNC pulse, by edge sequence, for pairing with `SyncEdge`.
    sync_pulses: [(u32, u64); 8],
    sync_pulse_next: usize,
    sync_seq: u32,
    last_samples_t: Option<u64>,
    pub dropped_samples: u32,
}

impl NodeHost {
    pub fn new(cfg: Config) -> Self {
        Self {
            clock: ClockMap::new(),
            detectors: core::array::from_fn(|_| None),
            cfg,
            hello: None,
            plan: None,
            pings: [None; 4],
            next_ping: 1,
            sync_pulses: [(0, 0); 8],
            sync_pulse_next: 0,
            sync_seq: 0,
            last_samples_t: None,
            dropped_samples: 0,
        }
    }

    pub fn clock(&self) -> &ClockMap {
        &self.clock
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

    /// Change detection settings for every seat (takes effect immediately).
    pub fn set_detect_config(&mut self, cfg: Config) {
        self.cfg = cfg;
        for d in self.detectors.iter_mut().flatten() {
            d.set_config(cfg);
        }
    }

    /// Record the plan the host sent to the Node, and create detectors for its seats.
    pub fn set_plan(&mut self, plan: Plan) {
        for d in self.detectors.iter_mut() {
            *d = None;
        }
        for &(seat, _) in plan.seats() {
            if let Some(slot) = self.detectors.get_mut(seat as usize) {
                *slot = Some(Detector::new(seat, self.cfg));
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
                self.sync_seq = 0;
                self.last_samples_t = None;
                out(Event::Hello(*h));
            }
            Message::Samples(s) => self.feed_samples(s, &mut out),
            Message::SyncEdge(e) => {
                if let Some(&(_, t_pulse)) = self.sync_pulses.iter().find(|(seq, _)| *seq == e.edge_seq) {
                    self.clock.add_pair(e.t_node, t_pulse);
                    out(Event::Synced);
                }
            }
            Message::Pong(p) => {
                let slot = (p.id as usize) % self.pings.len();
                if let Some(o) = self.pings[slot] {
                    if o.id == p.id && o.t_sent == p.t_timer {
                        self.pings[slot] = None;
                        if self.clock.add_ping(o.t_sent, t_timer, p.t_node) {
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
        det.floor_hint(s.noise_floor);
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
