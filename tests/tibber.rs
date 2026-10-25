//! Mapping tests on answers shaped like Tibber's, and the plugin against a
//! fake Tibber (GraphQL over wiremock, live data over a local websocket) with
//! HomewrightHQ played over a pipe.

mod host;

use std::time::Duration;

use chrono::{DateTime, Timelike, Utc};
use futures_util::{SinkExt, StreamExt};
use homewright_tibber::api::{ApiError, Client};
use homewright_tibber::model::{self, ws};
use homewright_tibber::Tibber;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;
use wiremock::matchers::{body_string_contains, header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

const H1: &str = "96a14971-525a-4420-aae9-e5aedaa129ff";
const H2: &str = "68e6938b-91a6-4199-a0d4-f24c22be87bb";

fn fixture(name: &str) -> Value {
    let p = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn data(name: &str) -> Value {
    fixture(name)["data"].clone()
}

#[test]
fn account_and_homes() {
    let v = model::parse_viewer(&data("viewer.json"));
    assert_eq!(v.user_id, "c7a9f1e2-0b3d-4e5f-8a6b-1c2d3e4f5a6b");
    assert_eq!(v.name, "Magnus Dunker");
    assert_eq!(v.websocket_url.as_deref(), Some("wss://websocket-api.tibber.com/v1-beta/gql/subscriptions"));
    assert_eq!(v.homes.len(), 2);
    assert_eq!(v.homes[0].name, "Hjemme");
    assert!(v.homes[0].realtime);
    // no nickname: the address is the name
    assert_eq!(v.homes[1].name, "Sommerhusvej 7");
    assert!(!v.homes[1].realtime);
    assert_eq!(model::short_id(H1), "96a14971");
}

#[test]
fn prices_now_today_and_tomorrow() {
    let d = data("prices.json");
    let p = model::parse_prices(model::home(&d, H1).unwrap());
    assert_eq!(p.today.len(), 24);
    assert_eq!(p.tomorrow.len(), 24);
    assert_eq!(p.unit(), "DKK/kWh");
    assert_eq!(p.today[23].end.to_rfc3339(), "2026-10-21T00:00:00+02:00");

    // 14:30 local time
    let now = at("2026-10-20T12:30:00Z");
    let (state, a) = model::price_now(&p, now).unwrap();
    assert_eq!(state, "1.675");
    assert_eq!(a["today"].as_array().unwrap().len(), 24);
    assert_eq!(a["today"][14], json!({ "start": "2026-10-20T14:00:00+02:00", "end": "2026-10-20T15:00:00+02:00", "price": 1.675 }));
    assert_eq!(a["tomorrow_valid"], true);
    assert_eq!(a["level"], "normal");
    assert_eq!(a["energy"], 0.62);
    assert_eq!(a["min_price"], 1.275);
    assert_eq!(a["max_price"], 2.8125);
    assert_eq!(model::price_level(&p, now).unwrap().0, "normal");

    let (avg, a) = model::day_price(&p.tomorrow).unwrap();
    assert_eq!(avg, "1.6922");
    assert_eq!(a["prices"].as_array().unwrap().len(), 24);
    assert_eq!(a["cheapest_start"], "2026-10-21T00:00:00+02:00");

    let other = model::parse_prices(model::home(&d, H2).unwrap());
    assert!(other.tomorrow.is_empty());
    assert_eq!(model::price_now(&other, now).unwrap().1["tomorrow_valid"], false);
    assert!(model::day_price(&other.tomorrow).is_none());
}

#[test]
fn quarter_hour_prices() {
    let d = data("prices_quarter.json");
    let p = model::parse_prices(model::home(&d, H1).unwrap());
    assert_eq!(p.today.len(), 96);
    assert_eq!((p.today[0].end - p.today[0].start).num_minutes(), 15);
    let cur = p.current(at("2026-10-20T12:37:00Z")).unwrap();
    assert_eq!(cur.start.to_rfc3339(), "2026-10-20T14:30:00+02:00");
    // the second home has no current price; the currency comes from today
    let other = model::parse_prices(model::home(&d, H2).unwrap());
    assert_eq!(other.currency.as_deref(), Some("DKK"));
}

#[test]
fn when_to_fetch_prices() {
    let d = data("prices.json");
    let full = model::parse_prices(model::home(&d, H1).unwrap());
    let no_tomorrow = model::parse_prices(model::home(&d, H2).unwrap());
    let now = at("2026-10-20T12:30:00Z");
    let ago = |m: i64| Some(now - chrono::Duration::minutes(m));
    assert!(full.fetch_due(now, None));
    assert!(!full.fetch_due(now, ago(60)));
    assert!(full.fetch_due(now, ago(7 * 60)));
    // after 13:00 without tomorrow's prices: every 15 minutes
    assert!(no_tomorrow.fetch_due(now, ago(20)));
    assert!(!no_tomorrow.fetch_due(now, ago(10)));
    // before 13:00 there is nothing to wait for
    let morning = at("2026-10-20T08:00:00Z");
    assert!(!no_tomorrow.fetch_due(morning, Some(morning - chrono::Duration::hours(1))));
    // the day has run out
    let midnight = at("2026-10-20T22:10:00Z");
    assert!(no_tomorrow.fetch_due(midnight, Some(midnight - chrono::Duration::minutes(6))));

    let mut rolled = full.clone();
    rolled.roll(at("2026-10-20T23:00:00Z"));
    assert_eq!(rolled.today, full.tomorrow);
    assert!(rolled.tomorrow.is_empty());
    assert_eq!(model::price_now(&rolled, at("2026-10-20T23:00:00Z")).unwrap().0, "1.1625");
}

#[test]
fn consumption() {
    let d = data("consumption.json");
    let usage = model::parse_usage(model::home(&d, H1).unwrap());
    // hours without a reading are left out
    assert_eq!(usage.len(), 37);
    let (state, a) = model::last_hour(&usage).unwrap();
    assert_eq!(state, "0.4");
    assert_eq!(a["from"], "2026-10-20T12:00:00+02:00");
    assert_eq!(a["cost"], 0.76);
    assert_eq!(model::usage_today(&usage, at("2026-10-20T11:30:00Z")), Some((5.8, Some(11.02))));
    assert_eq!(model::usage_currency(&usage).as_deref(), Some("DKK"));
    let none = model::parse_usage(model::home(&d, H2).unwrap());
    assert!(model::last_hour(&none).is_none());
    assert!(model::usage_today(&none, at("2026-10-20T11:30:00Z")).is_none());
}

#[test]
fn live_messages() {
    let msg = fixture("live.json").to_string();
    let ws::Incoming::Data(id, m) = ws::parse(&msg) else { panic!("ingen data") };
    assert_eq!(id, H1);
    let (state, a) = model::live_power(&m).unwrap();
    assert_eq!(state, "1543");
    assert_eq!(a["voltage_phase1"], 231.2);
    assert_eq!(a["current_l2"], 3.1);
    assert_eq!(a["signal_strength"], -71);
    assert_eq!(model::live_number(&m, "accumulatedConsumption", 3).unwrap().0, "9.871");
    assert_eq!(model::live_number(&m, "lastMeterConsumption", 3).unwrap().0, "41234.567");

    assert_eq!(ws::parse(r#"{"type":"connection_ack"}"#), ws::Incoming::Ack);
    assert_eq!(ws::parse(r#"{"type":"ping"}"#), ws::Incoming::Ping);
    assert!(matches!(ws::parse(r#"{"type":"error","id":"x","payload":[{"message":"nope"}]}"#), ws::Incoming::Error(_)));
    assert!(matches!(ws::parse(r#"{"type":"complete","id":"x"}"#), ws::Incoming::Error(_)));

    let init: Value = serde_json::from_str(&ws::init("tok")).unwrap();
    assert_eq!(init, json!({ "type": "connection_init", "payload": { "token": "tok" } }));
    let sub: Value = serde_json::from_str(&ws::subscribe(H1)).unwrap();
    assert_eq!(sub["type"], "subscribe");
    assert!(sub["payload"]["query"].as_str().unwrap().contains(&format!("liveMeasurement(homeId: \"{H1}\")")));
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_from_the_api() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(fixture("auth_error.json"))).mount(&server).await;
    assert_eq!(Client::new(&server.uri(), "bad").viewer().await.unwrap_err(), ApiError::Auth);

    let limited = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "120")).mount(&limited).await;
    assert_eq!(Client::new(&limited.uri(), "tok").viewer().await.unwrap_err(), ApiError::RateLimited(Some(Duration::from_secs(120))));
}

/// Today's prices around the real clock: 24 hourly slots, the current one
/// priced 1.5 and the others 1.0 + hour/100.
fn prices_now() -> Value {
    let now = Utc::now();
    let hour = now.with_minute(0).unwrap().with_second(0).unwrap().with_nanosecond(0).unwrap();
    let start = hour - chrono::Duration::hours(12);
    let today: Vec<Value> = (0..24)
        .map(|i| {
            let t = start + chrono::Duration::hours(i);
            let total = if i == 12 { 1.5 } else { 1.0 + i as f64 / 100.0 };
            json!({ "total": total, "energy": total * 0.4, "tax": total * 0.6, "startsAt": t.to_rfc3339(), "level": "CHEAP", "currency": "SEK" })
        })
        .collect();
    let info = |t: &Vec<Value>| json!({ "currentSubscription": { "priceInfo": { "current": t[12], "today": t, "tomorrow": [] } } });
    let mut h1 = info(&today);
    h1["id"] = json!(H1);
    let mut h2 = info(&today);
    h2["id"] = json!(H2);
    json!({ "data": { "viewer": { "homes": [h1, h2] } } })
}

async fn cloud(server: &MockServer, ws_url: &str) {
    let mut viewer = fixture("viewer.json");
    viewer["data"]["viewer"]["websocketSubscriptionUrl"] = json!(ws_url);
    let ok = |body: Value| ResponseTemplate::new(200).set_body_json(body);
    Mock::given(method("POST")).and(header("authorization", "Bearer tok")).and(body_string_contains("websocketSubscriptionUrl")).respond_with(ok(viewer)).mount(server).await;
    Mock::given(method("POST")).and(header("authorization", "Bearer tok")).and(body_string_contains("priceInfo")).respond_with(ok(prices_now())).mount(server).await;
    Mock::given(method("POST")).and(header("authorization", "Bearer tok")).and(body_string_contains("consumption(")).respond_with(ok(fixture("consumption.json"))).mount(server).await;
    Mock::given(method("POST")).respond_with(ok(fixture("auth_error.json"))).mount(server).await;
}

/// A websocket that speaks graphql-transport-ws: acks the token, answers the
/// subscription for the home with one measurement and stays open.
// the callback signature is tungstenite's, its error type is not ours to shrink
#[allow(clippy::result_large_err)]
async fn live_server() -> (String, tokio::sync::mpsc::UnboundedReceiver<Value>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/subscriptions", listener.local_addr().unwrap());
    let (seen, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut stream = tokio_tungstenite::accept_hdr_async(tcp, |req: &Request, mut res: Response| {
            assert_eq!(req.headers()["sec-websocket-protocol"], "graphql-transport-ws");
            assert!(req.headers()["user-agent"].to_str().unwrap().starts_with("homewright-tibber/"));
            res.headers_mut().insert("sec-websocket-protocol", "graphql-transport-ws".parse().unwrap());
            Ok(res)
        })
        .await
        .unwrap();
        while let Some(Ok(Message::Text(t))) = stream.next().await {
            let v: Value = serde_json::from_str(&t).unwrap();
            let _ = seen.send(v.clone());
            match v["type"].as_str().unwrap() {
                "connection_init" => stream.send(Message::Text(json!({ "type": "connection_ack" }).to_string())).await.unwrap(),
                "subscribe" => stream.send(Message::Text(fixture("live.json").to_string())).await.unwrap(),
                _ => {}
            }
        }
    });
    (url, rx)
}

fn flow(data: Value) -> Value {
    json!({ "flow_id": "f1", "source": "user", "data": data })
}

#[tokio::test(flavor = "multi_thread")]
async fn flow_setup_and_live_power() {
    let server = MockServer::start().await;
    let (ws_url, mut seen) = live_server().await;
    cloud(&server, &ws_url).await;
    let h = host::FakeHost::start(Tibber::default());
    let base = json!({ "base_url": server.uri() });

    let r = h.call("config_flow", json!({ "flow": flow(base.clone()), "step": "user", "input": null })).await.unwrap();
    assert_eq!(r["result"]["type"], "form");
    let r = h.call("config_flow", json!({ "flow": flow(base.clone()), "step": "user", "input": { "token": "wrong" } })).await.unwrap();
    assert_eq!(r["result"]["errors"]["base"], "invalid_auth");
    let r = h.call("config_flow", json!({ "flow": flow(base.clone()), "step": "user", "input": { "token": " tok " } })).await.unwrap();
    assert_eq!(r["result"]["type"], "create_entry");
    assert_eq!(r["result"]["title"], "Tibber (Magnus Dunker)");
    assert_eq!(r["unique_id"], "c7a9f1e2-0b3d-4e5f-8a6b-1c2d3e4f5a6b");
    let data = r["result"]["data"].clone();
    assert_eq!(data["token"], "tok");

    h.call("setup_entry", json!({ "entry": { "entry_id": "e1", "domain": "tibber", "data": data } })).await.unwrap();
    assert_eq!(h.rec.lock().unwrap().devices.len(), 2);

    let price = h.entity(&format!("{H1}-price"));
    let (state, attrs) = h.state(&price).unwrap();
    assert_eq!(state, "1.5");
    assert_eq!(attrs["today"].as_array().unwrap().len(), 24);
    assert_eq!(attrs["tomorrow_valid"], false);
    let (spec, _) = h.rec.lock().unwrap().entities[&price].clone();
    assert_eq!(spec["unit_of_measurement"], "SEK/kWh");
    assert_eq!(h.state(&h.entity(&format!("{H1}-price_level"))).unwrap().0, "cheap");
    assert_eq!(h.state(&h.entity(&format!("{H1}-price_tomorrow"))).unwrap().0, "unknown");
    assert_eq!(h.state(&h.entity(&format!("{H1}-consumption_last_hour"))).unwrap().0, "0.4");
    let cost = h.entity(&format!("{H1}-cost_today"));
    assert_eq!(h.rec.lock().unwrap().entities[&cost].0["unit_of_measurement"], "DKK");

    // only the home with Pulse has live sensors
    let power = h.entity(&format!("{H1}-power"));
    assert!(!h.rec.lock().unwrap().entities.keys().any(|k| k.contains(&model::short_id(H2)) && k.ends_with("_power")));
    let mut state = None;
    for _ in 0..100 {
        state = h.state(&power);
        if state.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (state, attrs) = state.expect("ingen live-effekt");
    assert_eq!(state, "1543");
    assert_eq!(attrs["voltage_phase2"], 229.8);
    assert_eq!(h.state(&h.entity(&format!("{H1}-meter_consumption"))).unwrap().0, "41234.567");

    let init = seen.recv().await.unwrap();
    assert_eq!(init["payload"]["token"], "tok");
    let sub = seen.recv().await.unwrap();
    assert_eq!(sub["id"], H1);

    let diag = h.call("diagnostics", json!({ "entry": { "entry_id": "e1", "data": { "token": "tok" } } })).await.unwrap();
    assert_eq!(diag["entry"]["token"], "**REDACTED**");
    assert_eq!(diag["homes"][0]["prices_today"], 24);

    let opts = h
        .call("options_flow", json!({ "flow": { "flow_id": "o1", "source": "options", "entry": { "entry_id": "e1", "data": {} } }, "step": "init", "input": { "resolution": "quarter_hourly" } }))
        .await
        .unwrap();
    assert_eq!(opts["result"]["data"]["resolution"], "quarter_hourly");

    // a revoked token fails setup so HomewrightHQ asks for a new one
    let err = h.call("setup_entry", json!({ "entry": { "entry_id": "e2", "domain": "tibber", "data": { "token": "old", "base_url": server.uri() } } })).await.unwrap_err();
    assert_eq!(err["data"]["kind"], "auth_failed");
    h.call("unload_entry", json!({ "entry_id": "e1" })).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn quarter_hour_option_asks_for_quarter_prices() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(fixture("prices_quarter.json"))).mount(&server).await;
    Client::new(&server.uri(), "tok").prices(true).await.unwrap();
    Client::new(&server.uri(), "tok").prices(false).await.unwrap();
    let bodies: Vec<String> = server.received_requests().await.unwrap().iter().map(|r| String::from_utf8_lossy(&r.body).to_string()).collect();
    assert!(bodies[0].contains("priceInfo(resolution: QUARTER_HOURLY)"));
    assert!(!bodies[1].contains("resolution"));
}
