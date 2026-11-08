//! Fake Node ↔ NodeHost over the real byte-level link: plan, SYNC, pings, samples, and
//! crossings that must land on the Timer clock within budget of the true pass centres.

use oft_core::{DetectConfig, Event, NodeHost};
use oft_nodelink::msg::{Message, Plan};
use oft_nodelink::{encode_frame, FrameDecoder, MAX_FRAME};
use oft_sim::fake_node::FakeNode;
use oft_sim::{NodeClock, Pass, Scene};

struct Rig {
    node: FakeNode,
    host: NodeHost,
    dec: FrameDecoder,
    events: Vec<Event>,
}

impl Rig {
    fn new(scene: Scene, clock: NodeClock) -> Self {
        Self { node: FakeNode::new(scene, clock, 7), host: NodeHost::new(DetectConfig::default()), dec: FrameDecoder::new(), events: Vec::new() }
    }

    fn send(&mut self, m: &Message, t_timer: u64) {
        let mut buf = [0u8; MAX_FRAME];
        let n = encode_frame(m, &mut buf).unwrap();
        self.node.write(&buf[..n], t_timer);
    }

    /// Pull everything the Node wrote and feed it to the host.
    fn pump(&mut self, t_timer: u64) {
        for b in self.node.take_output() {
            if let Some(r) = self.dec.push(b) {
                let m = r.expect("fake node frame must decode");
                let events = &mut self.events;
                self.host.feed(&m, t_timer, |e| events.push(e));
            }
        }
    }

    fn crossings(&self) -> Vec<oft_core::Crossing> {
        self.events.iter().filter_map(|e| if let Event::Crossing(c) = e { Some(*c) } else { None }).collect()
    }
}

fn single_seat_plan(freq: u16) -> Plan {
    let mut p = Plan::default();
    p.push(0, freq).unwrap();
    p
}

#[test]
fn hello_then_plan_then_samples_flow() {
    let mut rig = Rig::new(Scene::default(), NodeClock { offset_us: -1_000_000, ppm: 0.0 });
    rig.pump(0);
    assert!(matches!(rig.events.as_slice(), [Event::Hello(h)] if h.max_seats == 8));

    let plan = single_seat_plan(5800);
    rig.host.set_plan(plan);
    rig.send(&Message::Plan(plan), 1000);
    assert_eq!(rig.node.plan().unwrap().seats(), &[(0, 5800)]);

    // 200 ms of samples arrive in 50-sample batches.
    rig.node.advance(201_000);
    rig.pump(201_000);
    assert_eq!(rig.host.dropped_samples, 0);
    assert!(rig.host.detector(0).unwrap().floor().is_some());
    assert!(rig.crossings().is_empty());
}

#[test]
fn passes_land_on_the_timer_clock_with_sync_wire() {
    // Place passes in TIMER time, convert to Node time for the scene.
    let clock = NodeClock { offset_us: -4_000_000, ppm: 35.0 };
    let true_timer: Vec<u64> = (0..6).map(|k| 3_000_000 + k * 7_000_000).collect();
    let scene = Scene {
        passes: true_timer.iter().map(|&tt| Pass { center_us: clock.node_time(tt), height_db: 38.0, width_us: 130_000.0, dip: 0.4 }).collect(),
        ..Scene::default()
    };
    let mut rig = Rig::new(scene, clock);
    rig.pump(0);
    let plan = single_seat_plan(5800);
    rig.host.set_plan(plan);
    rig.send(&Message::Plan(plan), 0);

    // Run 45 s: SYNC at 20 Hz, pump every 20 ms.
    let mut t = 0u64;
    while t < 45_000_000 {
        t += 20_000;
        if t % 50_000 == 0 {
            let _seq = rig.host.sync_pulsed(t);
            rig.node.sync_edge(t);
        }
        rig.node.advance(t);
        rig.pump(t);
    }
    assert!(rig.host.clock().is_synced());
    assert!(rig.host.clock().residual_us() < 10.0, "residual {}", rig.host.clock().residual_us());
    assert!((rig.host.clock().drift_ppm() - 35.0).abs() < 3.0, "drift {}", rig.host.clock().drift_ppm());

    let found = rig.crossings();
    assert_eq!(found.len(), 6, "found {}", found.len());
    for (c, &tt) in found.iter().zip(&true_timer) {
        let err = c.t_mid as i64 - tt as i64;
        assert!(err.abs() < 3_000, "crossing on Timer clock off by {err} µs");
        assert_eq!(c.seat, 0);
    }
    // Lap intervals, Timer clock, within drift + detection jitter of 7.000 s.
    for w in found.windows(2) {
        let iv = w[1].t_mid as i64 - w[0].t_mid as i64;
        assert!((iv - 7_000_000).abs() < 4_000, "interval {iv}");
    }
}

#[test]
fn ping_only_sync_still_times_passes_to_a_millisecond() {
    let clock = NodeClock { offset_us: -2_500_000, ppm: -20.0 };
    let true_timer: Vec<u64> = (0..4).map(|k| 4_000_000 + k * 6_000_000).collect();
    let scene = Scene {
        passes: true_timer.iter().map(|&tt| Pass { center_us: clock.node_time(tt), height_db: 36.0, width_us: 120_000.0, dip: 0.0 }).collect(),
        ..Scene::default()
    };
    let mut rig = Rig::new(scene, clock);
    rig.pump(0);
    let plan = single_seat_plan(5760);
    rig.host.set_plan(plan);
    rig.send(&Message::Plan(plan), 0);

    let mut t = 0u64;
    let mut syncs = 0;
    while t < 28_000_000 {
        t += 20_000;
        if t % 200_000 == 0 {
            let ping = rig.host.make_ping(t);
            // Link latency: ping takes 200 µs to arrive, pong 250 µs back.
            rig.send(&ping, t + 200);
            rig.node.advance(t);
            let before = rig.events.len();
            rig.pump(t + 450);
            syncs += rig.events[before..].iter().filter(|e| matches!(e, Event::Synced)).count();
        } else {
            rig.node.advance(t);
            rig.pump(t);
        }
    }
    assert!(syncs > 50, "synced {syncs}");
    let found = rig.crossings();
    assert_eq!(found.len(), 4);
    for (c, &tt) in found.iter().zip(&true_timer) {
        let err = c.t_mid as i64 - tt as i64;
        assert!(err.abs() < 3_500, "ping-synced crossing off by {err} µs");
    }
}

#[test]
fn node_reboot_resends_hello_and_resets_clock() {
    let mut rig = Rig::new(Scene::default(), NodeClock { offset_us: -1_000_000, ppm: 0.0 });
    rig.pump(0);
    let plan = single_seat_plan(5800);
    rig.host.set_plan(plan);
    rig.send(&Message::Plan(plan), 0);
    for k in 1..=10u64 {
        rig.host.sync_pulsed(k * 100_000);
        rig.node.sync_edge(k * 100_000);
    }
    rig.pump(1_000_000);
    assert!(rig.host.clock().is_synced());

    rig.send(&Message::Reset, 1_100_000);
    rig.pump(1_100_000);
    assert_eq!(rig.node.resets, 1);
    assert!(rig.node.plan().is_none(), "plan must be forgotten on reset");
    assert!(matches!(rig.events.last(), Some(Event::Hello(_))));
    assert!(!rig.host.clock().is_synced(), "clock map must reset on a new Hello");
}

#[test]
fn identify_and_dfu_reach_the_node() {
    let mut rig = Rig::new(Scene::default(), NodeClock { offset_us: 0, ppm: 0.0 });
    rig.send(&Message::Identify { duration_ms: 500 }, 10_000);
    assert!(rig.node.identifying_at(400_000));
    assert!(!rig.node.identifying_at(600_000));
    rig.send(&Message::Dfu, 20_000);
    assert_eq!(rig.node.dfu_requests, 1);
}

#[test]
fn samples_for_an_unplanned_seat_are_counted_as_dropped() {
    let mut host = NodeHost::new(DetectConfig::default());
    let mut p = Plan::default();
    p.push(0, 5800).unwrap();
    host.set_plan(p);
    let mut s = oft_nodelink::msg::Samples::new(3, 0, 1000, -920, Default::default());
    for _ in 0..10 {
        s.push(-915).unwrap();
    }
    host.feed(&Message::Samples(s), 0, |_| {});
    assert_eq!(host.dropped_samples, 10);
}
