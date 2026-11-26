//! The Timer side of several Node links (spec §4.1): one [`Link`] per Node, one detector
//! per seat, and the seat scheduler deciding which Node watches which seat.
//!
//! The host tells it which seats are wanted ([`Hosts::set_seats`]) and which Nodes are
//! answering ([`Hosts::set_up`]), feeds it every Node's messages ([`Hosts::feed`]), and
//! sends a Node its plan ([`Hosts::plan`]) whenever one of those calls says it changed.
//! Like [`crate::NodeHost`] it has no I/O of its own.

use crate::clock::ClockMap;
use crate::detect::{Config, Detector, Threshold};
use crate::host::{Event, Link, MAX_SEATS};
use crate::sched;
use oft_nodelink::msg::{Hello, Message, Plan, GAIN_AUTO};

/// A set of Nodes, one bit per Node number.
pub type NodeMask = u32;

pub struct Hosts<const N: usize> {
    links: [Link; N],
    /// Nodes that may be given seats: the host hears them.
    up: [bool; N],
    plans: [Plan; N],
    /// Wanted seats: the frequency of each.
    seat_freq: [Option<u16>; MAX_SEATS],
    seat_node: [Option<u8>; MAX_SEATS],
    detectors: [Option<Detector>; MAX_SEATS],
    cfg: Config,
    /// Per-seat threshold overrides; `None` uses `cfg.threshold`.
    seat_thresholds: [Option<Threshold>; MAX_SEATS],
}

impl<const N: usize> Hosts<N> {
    pub fn new(cfg: Config) -> Self {
        Self {
            links: core::array::from_fn(|_| Link::new()),
            up: [false; N],
            plans: [Plan::with(0, 0, GAIN_AUTO); N],
            seat_freq: [None; MAX_SEATS],
            seat_node: [None; MAX_SEATS],
            detectors: core::array::from_fn(|_| None),
            cfg,
            seat_thresholds: [None; MAX_SEATS],
        }
    }

    /// Node `n`'s link: its `Hello`, clock and lost-sample count.
    pub fn link(&self, n: usize) -> &Link {
        &self.links[n]
    }

    pub fn hello(&self, n: usize) -> Option<&Hello> {
        self.links.get(n).and_then(|l| l.hello())
    }

    pub fn clock(&self, n: usize) -> &ClockMap {
        self.links[n].clock()
    }

    pub fn is_up(&self, n: usize) -> bool {
        self.up.get(n).copied().unwrap_or(false)
    }

    /// Nodes that are up.
    pub fn up_count(&self) -> usize {
        self.up.iter().filter(|&&u| u).count()
    }

    /// The plan Node `n` should be running. Empty for a Node with no seats.
    pub fn plan(&self, n: usize) -> Plan {
        self.plans[n]
    }

    /// The Node serving `seat`, if any.
    pub fn seat_node(&self, seat: u8) -> Option<u8> {
        self.seat_node.get(seat as usize).copied().flatten()
    }

    pub fn detector(&self, seat: u8) -> Option<&Detector> {
        self.detectors.get(seat as usize).and_then(|d| d.as_ref())
    }

    /// Samples lost on every link together.
    pub fn dropped_samples(&self) -> u32 {
        self.links.iter().fold(0u32, |a, l| a.wrapping_add(l.dropped_samples))
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

    /// The seats to watch, as `(seat, freq_mhz)`. Returns the Nodes whose plan changed: send
    /// each its [`Hosts::plan`]. Seats whose Node and frequency stay the same keep their
    /// detector, floor and all.
    pub fn set_seats(&mut self, seats: impl IntoIterator<Item = (u8, u16)>) -> NodeMask {
        self.seat_freq = [None; MAX_SEATS];
        for (seat, freq) in seats {
            if let Some(slot) = self.seat_freq.get_mut(seat as usize) {
                *slot = Some(freq);
            }
        }
        self.replan()
    }

    /// Say whether Node `n` is answering. A Node that is not gets no seats; the others take
    /// them over. Returns the Nodes whose plan changed.
    pub fn set_up(&mut self, n: usize, up: bool) -> NodeMask {
        match self.up.get_mut(n) {
            Some(slot) if *slot != up => *slot = up,
            _ => return 0,
        }
        self.replan()
    }

    fn replan(&mut self) -> NodeMask {
        let mut wanted = [(0u8, 0u16); MAX_SEATS];
        let mut n = 0;
        for (seat, freq) in self.seat_freq.iter().enumerate() {
            if let Some(freq) = freq {
                wanted[n] = (seat as u8, *freq);
                n += 1;
            }
        }
        let nodes: [Option<u8>; N] = core::array::from_fn(|k| {
            self.up[k].then(|| match self.links[k].hello().map(|h| h.max_seats) {
                // Not said yet, or nonsense: assume a full plan fits.
                None | Some(0) => MAX_SEATS as u8,
                Some(m) => m,
            })
        });
        let assigned = sched::assign(&wanted[..n], &nodes, &self.seat_node);

        let mut plans = [Plan::with(0, 0, GAIN_AUTO); N];
        for seat in 0..MAX_SEATS {
            let was = (self.seat_node[seat], self.plan_freq(seat as u8));
            let now = (assigned[seat], assigned[seat].and(self.seat_freq[seat]));
            if let (Some(node), Some(freq)) = now {
                let _ = plans[node as usize].push(seat as u8, freq);
            }
            if was != now {
                let cfg = self.seat_detect_config(seat as u8);
                self.detectors[seat] = now.0.map(|_| Detector::new(seat as u8, cfg));
            }
        }
        self.seat_node = assigned;
        let mut changed = 0;
        for k in 0..N {
            if plans[k] != self.plans[k] {
                changed |= 1 << k;
            }
        }
        self.plans = plans;
        changed
    }

    /// The frequency `seat` has in the plan now in force.
    fn plan_freq(&self, seat: u8) -> Option<u16> {
        let node = self.seat_node(seat)?;
        self.plans[node as usize].seats().iter().find(|s| s.0 == seat).map(|s| s.1)
    }

    /// Build a `Ping` for Node `n` to send now.
    pub fn make_ping(&mut self, n: usize, t_timer: u64) -> Message {
        self.links[n].make_ping(t_timer)
    }

    /// The host pulsed SYNC, one wire to every Node, at `t_timer`.
    pub fn sync_pulsed(&mut self, t_timer: u64) {
        for l in self.links.iter_mut() {
            l.sync_pulsed(t_timer);
        }
    }

    /// Feed one decoded message from Node `n`. `t_timer` is when it arrived. Events are
    /// pushed to `out`. Returns the Nodes that need their plan sent: `n` itself after a
    /// `Hello` (it has just started and has none), and any Node whose plan that changed.
    pub fn feed(&mut self, n: usize, msg: &Message, t_timer: u64, mut out: impl FnMut(Event)) -> NodeMask {
        let Some(link) = self.links.get_mut(n) else { return 0 };
        match msg {
            Message::Samples(s) => {
                // Only from the Node the seat is on: one still running an old plan must not
                // feed another Node's detector.
                let mine = self.seat_node.get(s.seat as usize).copied().flatten() == Some(n as u8);
                let det = if mine { self.detectors[s.seat as usize].as_mut() } else { None };
                link.feed_samples(det, s, &mut out);
                0
            }
            Message::Hello(_) => {
                link.feed(msg, t_timer, &mut out);
                // What it can take may have changed (other firmware, another board).
                let changed = if self.up[n] { self.replan() } else { 0 };
                changed | (1 << n)
            }
            _ => {
                link.feed(msg, t_timer, &mut out);
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oft_nodelink::msg::{Features, SampleFlags, Samples};

    fn hello(chip_id: u64, max_seats: u8) -> Message {
        Message::Hello(Hello { chip_id, fw: [0, 1, 0], link_version: 1, freq_min_mhz: 5645, freq_max_mhz: 5945, max_seats, features: Features::SYNC, chip: Default::default() })
    }

    fn seats_of(p: Plan) -> std::vec::Vec<(u8, u16)> {
        p.seats().to_vec()
    }

    #[test]
    fn nothing_is_planned_until_a_node_is_up() {
        let mut h = Hosts::<2>::new(Config::default());
        assert_eq!(h.set_seats([(0, 5658), (1, 5917)]), 0);
        assert_eq!(h.seat_node(0), None);
        assert!(h.detector(0).is_none());
        // The first Node takes both seats.
        assert_eq!(h.set_up(0, true), 0b01);
        assert_eq!(seats_of(h.plan(0)), [(0, 5658), (1, 5917)]);
        // The second takes one over; both plans change.
        assert_eq!(h.set_up(1, true), 0b11);
        assert_eq!(seats_of(h.plan(0)), [(0, 5658)]);
        assert_eq!(seats_of(h.plan(1)), [(1, 5917)]);
        assert_eq!(h.set_up(1, true), 0);
    }

    #[test]
    fn a_silent_node_loses_its_seats_and_gets_them_back() {
        let mut h = Hosts::<2>::new(Config::default());
        h.set_seats([(0, 5658), (1, 5917)]);
        h.set_up(0, true);
        h.set_up(1, true);
        assert_eq!(h.set_up(1, false), 0b11);
        assert_eq!(h.seat_node(1), Some(0));
        assert!(seats_of(h.plan(1)).is_empty());
        assert_eq!(h.set_up(1, true), 0b11);
        assert_eq!((h.seat_node(0), h.seat_node(1)), (Some(0), Some(1)));
    }

    #[test]
    fn a_hello_always_asks_for_that_nodes_plan() {
        let mut h = Hosts::<2>::new(Config::default());
        h.set_seats([(0, 5800)]);
        h.set_up(0, true);
        assert_eq!(h.feed(0, &hello(1, 8), 0, |_| {}), 0b01);
        // From a Node that is not up yet, too: the host marks it up next.
        assert_eq!(h.feed(1, &hello(2, 8), 0, |_| {}), 0b10);
        assert_eq!(h.hello(1).unwrap().chip_id, 2);
    }

    #[test]
    fn a_seat_that_does_not_move_keeps_its_detector() {
        let mut h = Hosts::<2>::new(Config::default());
        h.set_seats([(0, 5658)]);
        h.set_up(0, true);
        let mut s = Samples::new(0, 0, 1000, -900, SampleFlags::empty());
        for _ in 0..50 {
            s.push(-900).unwrap();
        }
        h.feed(0, &Message::Samples(s), 0, |_| {});
        let floor = h.detector(0).unwrap().floor();
        assert!(floor.is_some());
        // A second seat and a second Node arrive: seat 0 stays where it is.
        assert_eq!(h.set_seats([(0, 5658), (1, 5917)]), 0b01);
        assert_eq!(h.set_up(1, true), 0b11);
        assert_eq!(h.seat_node(0), Some(0));
        assert_eq!(h.detector(0).unwrap().floor(), floor);
        // A new frequency is a new detector.
        h.set_seats([(0, 5695), (1, 5917)]);
        assert_eq!(h.detector(0).unwrap().floor(), None);
    }

    #[test]
    fn samples_from_the_wrong_node_are_dropped() {
        let mut h = Hosts::<2>::new(Config::default());
        h.set_seats([(0, 5658), (1, 5917)]);
        h.set_up(0, true);
        h.set_up(1, true);
        let mut s = Samples::new(1, 0, 1000, -900, SampleFlags::empty());
        for _ in 0..10 {
            s.push(-900).unwrap();
        }
        // Seat 1 is on Node 1; Node 0 still sending it (an old plan) is not believed.
        h.feed(0, &Message::Samples(s), 0, |_| {});
        assert_eq!(h.link(0).dropped_samples, 10);
        assert_eq!(h.detector(1).unwrap().floor(), None);
        h.feed(1, &Message::Samples(s), 0, |_| {});
        assert!(h.detector(1).unwrap().floor().is_some());
        assert_eq!(h.dropped_samples(), 10);
    }

    #[test]
    fn a_node_with_one_receiver_gets_one_seat() {
        let mut h = Hosts::<2>::new(Config::default());
        h.feed(0, &hello(1, 1), 0, |_| {});
        h.feed(1, &hello(2, 8), 0, |_| {});
        h.set_up(0, true);
        h.set_up(1, true);
        h.set_seats([(0, 5658), (1, 5695), (2, 5732)]);
        assert_eq!(h.plan(0).seats().len(), 1);
        assert_eq!(h.plan(1).seats().len(), 2);
    }

    #[test]
    fn each_node_has_its_own_clock() {
        use oft_nodelink::msg::SyncEdge;
        let mut h = Hosts::<2>::new(Config::default());
        for k in 1..=10u64 {
            let t = k * 50_000;
            h.sync_pulsed(t);
            h.feed(0, &Message::SyncEdge(SyncEdge { edge_seq: k as u32, t_node: t + 1_000_000 }), t + 300, |_| {});
            h.feed(1, &Message::SyncEdge(SyncEdge { edge_seq: k as u32, t_node: t + 7_000_000 }), t + 400, |_| {});
        }
        assert_eq!(h.clock(0).to_timer(1_400_000), 400_000);
        assert_eq!(h.clock(1).to_timer(7_400_000), 400_000);
        assert_eq!(h.link(1).sync_source(), "sync");
    }
}
