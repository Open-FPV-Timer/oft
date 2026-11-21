//! HTTP + WebSocket handlers for the OFT Protocol (spec §6.3, §6.4).

use crate::protocol::{Config, Sub, API_PREFIX};
use crate::timer::{Outgoing, Timer};
use axum::extract::ws::{Message as WsMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub fn router(timer: Arc<Timer>) -> Router {
    let api = Router::new()
        .route("/info", get(info))
        .route("/config", get(get_config).put(put_config))
        .route("/config/save", get(get_config_save).post(post_config_save).delete(delete_config_save))
        .route("/status", get(status))
        .route("/time", get(time))
        .route("/crossings", get(crossings))
        .route("/samples", get(samples))
        .route("/stream", get(stream));
    Router::new().nest(API_PREFIX, api).route("/", get(root)).with_state(timer)
}

/// The built-in diagnostics page (the same file every Timer host serves).
async fn root() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("../../../web/timer.html"))
}

async fn info(State(t): State<Arc<Timer>>) -> Json<serde_json::Value> {
    Json(t.info())
}

async fn get_config(State(t): State<Arc<Timer>>) -> Json<Config> {
    Json(t.inner.lock().unwrap().config.clone())
}

async fn put_config(State(t): State<Arc<Timer>>, Json(c): Json<Config>) -> Response {
    if let Err(e) = c.check() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
    }
    t.set_config(c.clone());
    Json(c).into_response()
}

/// `{"saved", "current_is_saved"}` (spec §6.3), or 404 on a Timer started without a
/// settings file.
fn save_state(t: &Timer) -> Response {
    if t.cfg.settings.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "this Timer does not keep settings (oftd was started without a settings file)"}))).into_response();
    }
    let inner = t.inner.lock().unwrap();
    Json(json!({ "saved": inner.saved.is_some(), "current_is_saved": inner.saved.as_ref() == Some(&inner.config) })).into_response()
}

async fn get_config_save(State(t): State<Arc<Timer>>) -> Response {
    save_state(&t)
}

/// Store the config in use as the power-on config.
async fn post_config_save(State(t): State<Arc<Timer>>) -> Response {
    if t.cfg.settings.is_none() {
        return save_state(&t);
    }
    let config = t.inner.lock().unwrap().config.clone();
    match t.save_config(Some(config)) {
        Ok(()) => save_state(&t),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

/// Forget the power-on config: the built-in defaults at the next start.
async fn delete_config_save(State(t): State<Arc<Timer>>) -> Response {
    if t.cfg.settings.is_none() {
        return save_state(&t);
    }
    match t.save_config(None) {
        Ok(()) => save_state(&t),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

async fn status(State(t): State<Arc<Timer>>) -> Json<crate::protocol::StatusJson> {
    Json(t.status())
}

async fn time(State(t): State<Arc<Timer>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "time_us": t.now_us(), "boot_id": t.boot_id }))
}

#[derive(Deserialize)]
struct SinceQ {
    #[serde(default)]
    since: u64,
}

async fn crossings(State(t): State<Arc<Timer>>, Query(q): Query<SinceQ>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "boot_id": t.boot_id, "crossings": t.crossings_since(q.since) }))
}

#[derive(Deserialize)]
struct SamplesQ {
    seat: u8,
    from: u64,
    to: u64,
}

async fn samples(State(t): State<Arc<Timer>>, Query(q): Query<SamplesQ>) -> Response {
    match t.samples(q.seat, q.from, q.to) {
        Some(s) => Json(s).into_response(),
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "no samples for that seat and window"}))).into_response(),
    }
}

async fn stream(ws: WebSocketUpgrade, State(t): State<Arc<Timer>>) -> Response {
    ws.on_upgrade(move |socket| handle_ws(socket, t))
}

async fn handle_ws(socket: WebSocket, t: Arc<Timer>) {
    t.clients.fetch_add(1, Ordering::Relaxed);
    let (mut tx, mut rx) = socket.split();
    let mut events = t.events.subscribe();
    let mut sub = Sub::default();
    let mut decim_count: [u32; 8] = [0; 8];

    let hello = t.hello_json();
    if tx.send(WsMsg::Text(hello.to_string().into())).await.is_err() {
        t.clients.fetch_sub(1, Ordering::Relaxed);
        return;
    }

    loop {
        tokio::select! {
            incoming = rx.next() => {
                match incoming {
                    Some(Ok(WsMsg::Text(txt))) => {
                        match serde_json::from_str::<serde_json::Value>(&txt) {
                            Ok(v) if v.get("type").and_then(|x| x.as_str()) == Some("sub") => {
                                match serde_json::from_value::<Sub>(v) {
                                    Ok(s) => {
                                        // Replay missed crossings on request.
                                        if let Some(since) = s.crossings.as_ref().and_then(|c| c.since) {
                                            for c in t.crossings_since(since) {
                                                let _ = tx.send(WsMsg::Text(serde_json::to_string(&c).unwrap().into())).await;
                                            }
                                        }
                                        sub = s;
                                        let _ = tx.send(WsMsg::Text(serde_json::json!({"type":"ack","for":"sub"}).to_string().into())).await;
                                    }
                                    Err(e) => {
                                        let _ = tx.send(WsMsg::Text(serde_json::json!({"type":"error","code":"bad_sub","msg":e.to_string()}).to_string().into())).await;
                                    }
                                }
                            }
                            Ok(v) if v.get("type").and_then(|x| x.as_str()) == Some("ping") => {
                                let _ = tx.send(WsMsg::Text(serde_json::json!({"type":"pong","time_us":t.now_us()}).to_string().into())).await;
                            }
                            Ok(_) => {
                                let _ = tx.send(WsMsg::Text(serde_json::json!({"type":"error","code":"unknown_type"}).to_string().into())).await;
                            }
                            Err(e) => {
                                let _ = tx.send(WsMsg::Text(serde_json::json!({"type":"error","code":"bad_json","msg":e.to_string()}).to_string().into())).await;
                            }
                        }
                    }
                    Some(Ok(WsMsg::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
            ev = events.recv() => {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };
                let text = match &ev {
                    Outgoing::Crossing(c) if sub.crossings.is_some() => Some(serde_json::to_string(c).unwrap()),
                    Outgoing::Samples(s) => match &sub.samples {
                        Some(sel) if sel.seats.includes(s.seat) => {
                            let d = sel.decimate.max(1);
                            if d == 1 {
                                Some(serde_json::to_string(s).unwrap())
                            } else {
                                // Keep every d-th sample, stitched across batches.
                                let idx = s.seat as usize % 8;
                                let mut out = Vec::new();
                                let mut first_t0 = None;
                                for (k, &p) in s.p.iter().enumerate() {
                                    if decim_count[idx] == 0 {
                                        if first_t0.is_none() { first_t0 = Some(s.t0 + k as u64 * s.dt_us as u64); }
                                        out.push(p);
                                    }
                                    decim_count[idx] = (decim_count[idx] + 1) % d;
                                }
                                first_t0.map(|t0| serde_json::to_string(&crate::protocol::SamplesJson { kind: "samples", seat: s.seat, t0, dt_us: s.dt_us * d, p: out, nf: s.nf }).unwrap())
                            }
                        }
                        _ => None,
                    },
                    Outgoing::Status(st) if sub.status => Some(serde_json::to_string(st).unwrap()),
                    _ => None,
                };
                if let Some(text) = text {
                    if tx.send(WsMsg::Text(text.into())).await.is_err() { break; }
                }
            }
        }
    }
    t.clients.fetch_sub(1, Ordering::Relaxed);
}
