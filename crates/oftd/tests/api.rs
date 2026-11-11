//! The OFT Protocol over a real socket, against the fake Node: info, config, time,
//! status, crossings with replay, sample history, and the WebSocket stream.

use futures_util::{SinkExt, StreamExt};
use oft_sim::fake_node::FakeNode;
use oft_sim::{NodeClock, Pass, Scene};
use oftd::link::FakeLink;
use oftd::protocol::Config;
use oftd::{serve, Timer, TimerConfig};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMsg;

struct Server {
    base: String,
    ws_url: String,
    timer: Arc<Timer>,
    _server: tokio::task::JoinHandle<()>,
    _driver: tokio::task::JoinHandle<()>,
}

/// Start a Timer whose fake Node passes every `lap_s` seconds (first at 1.5 s),
/// with the driver ticking every 5 ms so tests run quickly.
async fn start(lap_s: f64) -> Server {
    let gap = (lap_s * 1e6) as u64;
    let n = if gap > 0 { 1000 } else { 0 };
    let scene = Scene {
        passes: (0..n).map(|k| Pass { center_us: 1_500_000 + k * gap, height_db: 36.0, width_us: 100_000.0, dip: 0.3 }).collect(),
        ..Scene::default()
    };
    let link = Box::new(FakeLink(FakeNode::new(scene, NodeClock { offset_us: -700_000, ppm: 25.0 }, 5)));
    let cfg = TimerConfig { id: "oft-test".into(), tick: Duration::from_millis(5), status_period: Duration::from_millis(200), ..TimerConfig::default() };
    // Short min-lap so 1 s test laps are all counted.
    let timer = Timer::new(cfg, link, Config { min_lap_ms: 500, ..Config::default() });
    let driver = tokio::spawn(timer.clone().run());
    let (addr, server) = serve(timer.clone(), "127.0.0.1:0".parse().unwrap()).await.unwrap();
    Server { base: format!("http://{addr}/oft/v1"), ws_url: format!("ws://{addr}/oft/v1/stream"), timer, _server: server, _driver: driver }
}

async fn get(url: &str) -> Value {
    reqwest::get(url).await.unwrap().json().await.unwrap()
}

#[tokio::test]
async fn info_advertises_capabilities_and_the_node() {
    let s = start(0.0).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let info = get(&format!("{}/info", s.base)).await;
    assert_eq!(info["id"], "oft-test");
    assert_eq!(info["proto"], 1);
    assert_eq!(info["level"], "L3");
    assert!(info["capabilities"]["crossings"]["algorithms"].as_array().unwrap().contains(&Value::from("edge_mid")));
    assert!(info["capabilities"]["samples.history"].is_object());
    assert!(info["capabilities"]["time"]["sync"].as_array().unwrap().contains(&Value::from("sync")));
    let nodes = info["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1, "fake node should have said hello: {info}");
    assert_eq!(nodes[0]["max_seats"], 8);
    assert_eq!(nodes[0]["link"], "fake");
}

#[tokio::test]
async fn time_and_status_report_a_synced_clock() {
    let s = start(0.0).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let t1 = get(&format!("{}/time", s.base)).await["time_us"].as_u64().unwrap();
    let t2 = get(&format!("{}/time", s.base)).await["time_us"].as_u64().unwrap();
    assert!(t2 >= t1);
    let st = get(&format!("{}/status", s.base)).await;
    assert_eq!(st["node_connected"], true);
    assert_eq!(st["clock_synced"], true, "{st}");
    assert_eq!(st["clock_source"], "sync", "{st}");
    assert!(st["clock_err_us"].as_f64().unwrap() < 50.0, "{st}");
    assert!((st["drift_ppm"].as_f64().unwrap() - 25.0).abs() < 5.0, "{st}");
    let seats = st["seats"].as_array().unwrap();
    assert_eq!(seats.len(), 1);
    assert_eq!(seats[0]["rate_hz"], 1000);
    assert!(seats[0]["rssi"].is_number());
    assert!(seats[0]["floor"].is_number());
}

#[tokio::test]
async fn crossings_appear_with_sequence_numbers_and_replay() {
    let s = start(1.0).await;
    // Passes at 1.5, 2.5, 3.5 s of fake-node time; wait for three.
    tokio::time::sleep(Duration::from_millis(4200)).await;
    let all = get(&format!("{}/crossings", s.base)).await;
    let list = all["crossings"].as_array().unwrap();
    assert!(list.len() >= 3, "got {}: {all}", list.len());
    assert_eq!(list[0]["seq"], 1);
    assert_eq!(list[1]["seq"], 2);
    assert_eq!(list[0]["seat"], 0);
    assert!(list[0]["snr"].as_i64().unwrap() > 250);
    assert_eq!(list[0]["type"], "crossing");
    // Lap interval on the Timer clock ≈ 1.000 s (drift 25 ppm ≈ 25 µs).
    let iv = list[1]["t_mid"].as_i64().unwrap() - list[0]["t_mid"].as_i64().unwrap();
    assert!((iv - 1_000_000).abs() < 4_000, "interval {iv}");
    // since= excludes what we have.
    let later = get(&format!("{}/crossings?since=2", s.base)).await;
    let later = later["crossings"].as_array().unwrap();
    assert_eq!(later[0]["seq"], 3);
    assert_eq!(later.len(), list.len() - 2);
}

#[tokio::test]
async fn sample_history_covers_a_crossing() {
    let s = start(1.0).await;
    tokio::time::sleep(Duration::from_millis(2200)).await;
    let c = &get(&format!("{}/crossings", s.base)).await["crossings"][0];
    let (from, to) = (c["t_enter"].as_u64().unwrap() - 200_000, c["t_exit"].as_u64().unwrap() + 200_000);
    let h = get(&format!("{}/samples?seat=0&from={from}&to={to}", s.base)).await;
    let p = h["p"].as_array().unwrap();
    assert!(p.len() > 300, "history has {} samples", p.len());
    assert_eq!(h["dt_us"], 1000);
    let peak = p.iter().map(|v| v.as_i64().unwrap()).max().unwrap();
    assert!(peak > -700, "peak {peak}");
    // Missing seat → 404.
    let r = reqwest::get(format!("{}/samples?seat=5&from=0&to=1", s.base)).await.unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn config_round_trips_and_replans_the_node() {
    let s = start(0.0).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let client = reqwest::Client::new();
    let mut cfg: Config = client.get(format!("{}/config", s.base)).send().await.unwrap().json().await.unwrap();
    assert_eq!(cfg.seats[0].freq_mhz, 5800);
    cfg.seats[0].freq_mhz = 5740;
    cfg.min_lap_ms = 3000;
    cfg.threshold = oftd::protocol::ThresholdConfig::Manual { enter: -760, exit: -800 };
    let r = client.put(format!("{}/config", s.base)).json(&cfg).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let back: Config = client.get(format!("{}/config", s.base)).send().await.unwrap().json().await.unwrap();
    assert_eq!(back, cfg);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let st = get(&format!("{}/status", s.base)).await;
    assert_eq!(st["seats"][0]["enter"], -760);
    assert_eq!(st["seats"][0]["exit"], -800);
    // Bad seat index is rejected.
    cfg.seats[0].seat = 9;
    let r = client.put(format!("{}/config", s.base)).json(&cfg).send().await.unwrap();
    assert_eq!(r.status(), 400);
    // Detector config really changed.
    let inner = s.timer.inner.lock().unwrap();
    assert_eq!(inner.host.detect_config().min_lap_us, 3_000_000);
}

#[tokio::test]
async fn seats_can_carry_their_own_threshold() {
    let s = start(0.0).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let client = reqwest::Client::new();
    let url = format!("{}/config", s.base);
    assert_eq!(get(&format!("{}/info", s.base)).await["capabilities"]["config"]["seat_thresholds"], true);

    // Seat 1 has its own manual levels; seat 0 uses the Timer-wide one; seat 2 says null.
    let body = serde_json::json!({
        "seats": [
            { "seat": 0, "freq_mhz": 5658, "enabled": true },
            { "seat": 1, "freq_mhz": 5732, "enabled": true, "threshold": { "mode": "manual", "enter": -700, "exit": -760 } },
            { "seat": 2, "freq_mhz": 5806, "enabled": true, "threshold": null },
        ],
        "seats_per_node": 8,
        "threshold": { "mode": "manual", "enter": -500, "exit": -550 },
        "min_lap_ms": 2000,
    });
    let r = client.put(&url).json(&body).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let back = get(&url).await;
    assert_eq!(back["threshold"], body["threshold"]);
    assert!(back["seats"][0].get("threshold").is_none(), "{back}");
    assert_eq!(back["seats"][1]["threshold"], body["seats"][1]["threshold"]);
    assert!(back["seats"][2].get("threshold").is_none(), "null reads back as absent: {back}");
    let st = get(&format!("{}/status", s.base)).await;
    assert_eq!((st["seats"][0]["enter"].as_i64(), st["seats"][0]["exit"].as_i64()), (Some(-500), Some(-550)), "{st}");
    assert_eq!((st["seats"][1]["enter"].as_i64(), st["seats"][1]["exit"].as_i64()), (Some(-700), Some(-760)), "{st}");
    assert_eq!(st["seats"][2]["enter"], -500);

    // A v0.3 client's PUT (no seat thresholds) still works and clears seat 1's own levels.
    let v03 = serde_json::json!({
        "seats": [{ "seat": 0, "freq_mhz": 5658, "enabled": true }, { "seat": 1, "freq_mhz": 5732, "enabled": true }],
        "seats_per_node": 8,
        "threshold": { "mode": "auto", "snr_enter": 120, "hysteresis": 40 },
        "min_lap_ms": 2000,
    });
    let r = client.put(&url).json(&v03).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(get(&url).await, v03);
    let inner = s.timer.inner.lock().unwrap();
    assert_eq!(inner.host.seat_threshold(1), None);
    assert_eq!(inner.host.detector(1).unwrap().config().threshold, oft_core::Threshold::Auto { snr_enter: 120, hysteresis: 40 });
}

#[tokio::test]
async fn discovery_txt_follows_the_timer() {
    let s = start(0.0).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let txt: std::collections::HashMap<_, _> = s.timer.discovery_txt().into_iter().collect();
    assert_eq!(txt["id"], "oft-test");
    assert_eq!(txt["proto"], "1");
    assert_eq!(txt["seats"], "1");
    assert_eq!(txt["nodes"], "1");
    assert_eq!(txt["level"], "L3");
    assert_eq!(txt["fw"], env!("CARGO_PKG_VERSION"));
}

/// Needs multicast on the host, so it is not part of the default run:
/// `cargo test -p oftd --test api mdns -- --ignored`.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mdns_advertises_the_timer() {
    use mdns_sd::{ServiceDaemon, ServiceEvent};
    let s = start(0.0).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let port = s.base.split(':').nth(2).unwrap().split('/').next().unwrap().parse::<u16>().unwrap();
    let adv = oftd::mdns::advertise(s.timer.clone(), format!("0.0.0.0:{port}").parse().unwrap()).unwrap();
    assert_eq!(adv.fullname(), "oft-test._oft._tcp.local.");

    let browser = ServiceDaemon::new().unwrap();
    let rx = browser.browse(oftd::mdns::SERVICE_TYPE).unwrap();
    let wait_for = |seats: &'static str| {
        let rx = rx.clone();
        async move {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let ServiceEvent::ServiceResolved(r) = rx.recv_async().await.unwrap() {
                        if r.fullname == "oft-test._oft._tcp.local." && r.txt_properties.get_property_val_str("seats") == Some(seats) {
                            return r;
                        }
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("service with seats={seats} not seen within 10 s"))
        }
    };
    let found = wait_for("1").await;
    assert_eq!(found.port, port);
    assert_eq!(found.host, "oft-test.local.");
    assert!(!found.addresses.is_empty());
    for (k, v) in [("id", "oft-test"), ("proto", "1"), ("nodes", "1"), ("level", "L3")] {
        assert_eq!(found.txt_properties.get_property_val_str(k), Some(v), "TXT {k}");
    }

    // The TXT record is re-announced when the seat count changes.
    let mut cfg = Config::default();
    cfg.seats.push(oftd::protocol::SeatConfig { seat: 1, freq_mhz: 5732, enabled: true, threshold: None });
    s.timer.set_config(cfg);
    wait_for("2").await;
    let _ = browser.shutdown();
    tokio::task::spawn_blocking(move || adv.shutdown()).await.unwrap();
}

#[tokio::test]
async fn websocket_hello_sub_and_live_crossings() {
    let s = start(1.0).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&s.ws_url).await.unwrap();
    let hello: Value = serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["timer"], "oft-test");
    assert!(hello["capabilities"]["crossings"].is_object());
    assert_eq!(hello["capabilities"]["config"]["seat_thresholds"], true);

    ws.send(WsMsg::Text(r#"{"type":"sub","crossings":{},"samples":{"seats":"all","decimate":10},"status":true}"#.into())).await.unwrap();
    let mut got_ack = false;
    let mut crossings = Vec::new();
    let mut samples = 0;
    let mut statuses = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    while tokio::time::Instant::now() < deadline && crossings.len() < 2 {
        let msg = match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(m))) => m,
            _ => break,
        };
        let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        match v["type"].as_str().unwrap() {
            "ack" => got_ack = true,
            "crossing" => crossings.push(v),
            "samples" => {
                samples += 1;
                assert_eq!(v["dt_us"], 10_000, "decimated dt");
            }
            "status" => statuses += 1,
            other => panic!("unexpected {other}"),
        }
    }
    assert!(got_ack);
    assert!(samples > 10, "samples {samples}");
    assert!(statuses >= 1);
    assert_eq!(crossings.len(), 2, "{crossings:?}");
    assert_eq!(crossings[0]["seq"], 1);
    assert_eq!(crossings[1]["seq"], 2);

    // A second client that reconnects with since=0 gets both replayed.
    let (mut ws2, _) = tokio_tungstenite::connect_async(&s.ws_url).await.unwrap();
    let _hello = ws2.next().await.unwrap().unwrap();
    ws2.send(WsMsg::Text(r#"{"type":"sub","crossings":{"since":0}}"#.into())).await.unwrap();
    let mut replayed = 0;
    for _ in 0..3 {
        let v: Value = serde_json::from_str(ws2.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        if v["type"] == "crossing" {
            replayed += 1;
        }
    }
    assert!(replayed >= 2, "replayed {replayed}");
    assert_eq!(s.timer.clients.load(std::sync::atomic::Ordering::Relaxed), 2);
}

#[tokio::test]
async fn websocket_rejects_bad_subscriptions_without_dropping() {
    let s = start(0.0).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&s.ws_url).await.unwrap();
    let _hello = ws.next().await.unwrap().unwrap();
    ws.send(WsMsg::Text("not json".into())).await.unwrap();
    let v: Value = serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(v["type"], "error");
    assert_eq!(v["code"], "bad_json");
    ws.send(WsMsg::Text(r#"{"type":"ping"}"#.into())).await.unwrap();
    let v: Value = serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(v["type"], "pong");
    assert!(v["time_us"].is_u64());
}
