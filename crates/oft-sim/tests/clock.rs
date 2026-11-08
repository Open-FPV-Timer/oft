//! Clock mapping end to end: a drifting Node, SYNC edges or pings, and a crossing that
//! must land on the Timer clock within the budget.

use oft_core::ClockMap;
use oft_sim::NodeClock;

#[test]
fn sync_wire_maps_a_crossing_to_under_100us() {
    // Node booted 12.3 s before the Timer and runs 40 ppm fast.
    let node = NodeClock { offset_us: -12_345_678, ppm: 40.0 };
    let mut map = ClockMap::new();
    // 20 Hz SYNC for 3 s.
    for k in 0..60u64 {
        let tt = 500_000 + k * 50_000;
        map.add_pair(node.node_time(tt), tt);
    }
    let true_timer = 4_200_000u64; // 700 ms after the last edge
    let t_node = node.node_time(true_timer);
    let mapped = map.to_timer(t_node);
    let err = mapped as i64 - true_timer as i64;
    assert!(err.abs() < 100, "sync-wire mapping error {err} µs");
    assert!(map.residual_us() < 5.0);
}

#[test]
fn ping_fit_over_a_jittery_link_stays_under_1ms() {
    let node = NodeClock { offset_us: -3_000_000, ppm: -25.0 };
    let mut map = ClockMap::new();
    let mut rng = oft_sim::Rng::new(99);
    // Pings every 200 ms for 10 s. RTT 300–600 µs, with 1 in 5 stalled to 5–20 ms.
    for k in 0..50u64 {
        let t_sent = 1_000_000 + k * 200_000;
        let rtt = if k % 5 == 2 { 5_000 + (rng.uniform() * 15_000.0) as u64 } else { 300 + (rng.uniform() * 300.0) as u64 };
        // Node answers somewhere inside the RTT, biased by asymmetric link delay.
        let t_reply = t_sent + rtt / 2 + ((rng.uniform() - 0.5) * (rtt as f64) * 0.4) as u64;
        map.add_ping(t_sent, t_sent + rtt, node.node_time(t_reply));
    }
    let true_timer = 12_000_000u64;
    let mapped = map.to_timer(node.node_time(true_timer));
    let err = mapped as i64 - true_timer as i64;
    assert!(err.abs() < 1_000, "ping-fit mapping error {err} µs");
}

#[test]
fn lap_interval_on_one_node_is_independent_of_mapping_offset() {
    // Two crossings 7.000000 s apart on the Node clock. Mapping must preserve the interval
    // to within the drift over that interval (40 ppm × 7 s = 280 µs).
    let node = NodeClock { offset_us: -777_777, ppm: 40.0 };
    let mut map = ClockMap::new();
    for k in 0..30u64 {
        let tt = k * 100_000;
        map.add_pair(node.node_time(tt), tt);
    }
    let a = node.node_time(10_000_000);
    let b = a + 7_000_000;
    let interval = map.to_timer(b) as i64 - map.to_timer(a) as i64;
    let expected = (7_000_000.0 / (1.0 + 40e-6)) as i64;
    assert!((interval - expected).abs() < 50, "interval {interval} vs {expected}");
}
