//! Tibber: electricity prices, consumption and Tibber Pulse live data, as an
//! external integration for HomewrightHQ.
//!
//! The user pastes a personal access token from developer.tibber.com. Every
//! home on the account becomes a device with a price sensor (the price now,
//! with today's and tomorrow's prices as attributes), the price level, the
//! average price today and tomorrow, and consumption and cost. A home with a
//! Pulse or Watty also gets live power over Tibber's websocket.

pub mod api;
pub mod model;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use homewright_plugin::{async_trait, json, Device, Entity, Entry, Error, Flow, FlowReply, Host, Integration, Map, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

use api::{ApiError, Client};
use model::{ws, HomeInfo, Prices, StateUpdate, Usage, Viewer};

pub const DOMAIN: &str = "tibber";
/// How often prices are checked against the clock. Fetching follows
/// [`Prices::fetch_due`].
const TICK: Duration = Duration::from_secs(60);
/// How often consumption is fetched, and how many hours back.
const USAGE_POLL: Duration = Duration::from_secs(3600);
const USAGE_HOURS: u32 = 48;
/// The pause after a 429 without Retry-After.
const RATE_LIMIT_PAUSE: Duration = Duration::from_secs(15 * 60);
/// Live data: the shortest time between two state updates of a home, how long
/// to wait for data before reconnecting, and the longest wait between tries.
const LIVE_MIN_INTERVAL: Duration = Duration::from_secs(5);
const LIVE_SILENCE: Duration = Duration::from_secs(90);
const LIVE_MAX_BACKOFF: Duration = Duration::from_secs(300);

#[doc(hidden)]
pub fn emit(level: &str, message: String) {
    homewright_plugin::log(level, message);
}

#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { $crate::emit("INFO", format!($($t)*)) };
}

#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => { $crate::emit("WARN", format!($($t)*)) };
}

fn token_schema() -> Value {
    json!({
        "type": "object",
        "required": ["token"],
        "properties": {
            "token": {
                "type": "string", "format": "password", "title": "Adgangsnøgle",
                "description": "Personlig adgangsnøgle fra developer.tibber.com (log ind med din Tibber-konto)"
            }
        }
    })
}

fn options_schema(quarter: bool) -> Value {
    json!({
        "type": "object",
        "properties": {
            "resolution": {
                "type": "string", "title": "Prisopløsning", "default": if quarter { "quarter_hourly" } else { "hourly" },
                "oneOf": [ { "const": "hourly", "title": "En pris pr. time" }, { "const": "quarter_hourly", "title": "En pris pr. kvarter" } ]
            }
        }
    })
}

fn base_url(get: impl Fn(&str) -> Option<String>) -> String {
    get("base_url").unwrap_or_else(|| api::BASE_URL.to_string())
}

fn error_code(e: &ApiError) -> &'static str {
    match e {
        ApiError::Auth => "invalid_auth",
        ApiError::RateLimited(_) => "rate_limited",
        ApiError::Other(_) => "cannot_connect",
    }
}

pub fn quarter_hourly(entry: &Entry) -> bool {
    entry.get_str("resolution") == Some("quarter_hourly")
}

#[derive(Default)]
pub struct Tibber {
    accounts: Mutex<HashMap<String, Arc<Account>>>,
}

#[async_trait]
impl Integration for Tibber {
    async fn config_flow(&self, _host: &Host, flow: Flow, step: &str, input: Option<Value>) -> Result<FlowReply, Error> {
        match (step, input) {
            ("user" | "reauth", None) => Ok(FlowReply::form("user", token_schema())),
            ("user", Some(v)) => {
                let token = v["token"].as_str().unwrap_or_default().trim().to_string();
                let base = base_url(|k| flow.data.get(k).and_then(Value::as_str).map(String::from));
                let viewer = match Client::new(&base, &token).viewer().await {
                    Ok(data) => model::parse_viewer(&data),
                    Err(e) => {
                        info!("{e}");
                        return Ok(FlowReply::form_with_errors("user", token_schema(), json!({ "base": error_code(&e) })));
                    }
                };
                if flow.source == "reauth" {
                    let mut data = flow.entry.as_ref().and_then(|e| e.data.as_object().cloned()).unwrap_or_default();
                    data.insert("token".into(), json!(token));
                    return Ok(FlowReply::create("", Value::Object(data)));
                }
                if viewer.homes.is_empty() {
                    return Ok(FlowReply::abort("no_homes"));
                }
                let mut data = flow.data.clone();
                data.insert("token".into(), json!(token));
                let title = if viewer.name.is_empty() { "Tibber".to_string() } else { format!("Tibber ({})", viewer.name) };
                let reply = FlowReply::create(&title, Value::Object(data));
                Ok(if viewer.user_id.is_empty() { reply } else { reply.with_unique_id(viewer.user_id) })
            }
            (s, _) => Err(Error::invalid(format!("ukendt trin {s}"))),
        }
    }

    async fn options_flow(&self, _host: &Host, flow: Flow, _step: &str, input: Option<Value>) -> Result<FlowReply, Error> {
        let current = flow.entry.as_ref().is_some_and(quarter_hourly);
        match input {
            None => Ok(FlowReply::form("init", options_schema(current))),
            Some(v) => {
                let r = if v["resolution"] == "quarter_hourly" { "quarter_hourly" } else { "hourly" };
                Ok(FlowReply::create("", json!({ "resolution": r })))
            }
        }
    }

    async fn setup_entry(&self, host: &Host, entry: Entry) -> Result<(), Error> {
        let entry_id = entry.entry_id.clone();
        let token = entry.get_str("token").filter(|t| !t.is_empty()).ok_or_else(|| Error::auth_failed("adgangsnøglen mangler"))?;
        let client = Arc::new(Client::new(&base_url(|k| entry.get_str(k).map(String::from)), token));
        let viewer: Viewer = match client.viewer().await {
            Ok(data) => model::parse_viewer(&data),
            Err(ApiError::Auth) => return Err(Error::auth_failed("Tibber afviste adgangsnøglen")),
            Err(e) => return Err(Error::not_ready(e)),
        };
        let _ = host.delete_issue(&entry_id, "auth").await;

        let account = Arc::new(Account {
            entry_id: entry_id.clone(),
            host: host.clone(),
            client,
            quarter: quarter_hourly(&entry),
            homes: Mutex::new(vec![]),
            entities: Mutex::new(vec![]),
            tasks: Mutex::new(vec![]),
            paused_until: Mutex::new(None),
            prices_fetched: Mutex::new(None),
            usage_fetched: Mutex::new(None),
        });
        for info in &viewer.homes {
            account.homes.lock().unwrap().push(Home { info: info.clone(), prices: Prices::default(), usage: vec![], live_published: None });
        }
        // fetch first, so the units carry the currency
        match account.fetch_prices(Utc::now()).await {
            Ok(()) => {}
            Err(ApiError::Auth) => return Err(Error::auth_failed("Tibber afviste adgangsnøglen")),
            Err(e) => return Err(Error::not_ready(e)),
        }
        if let Err(e) = account.fetch_usage().await {
            account.report(&e).await;
        }
        let has_ws = viewer.websocket_url.is_some();
        for info in &viewer.homes {
            let (price_unit, currency) = {
                let homes = account.homes.lock().unwrap();
                let h = homes.iter().find(|h| h.info.id == info.id);
                let price_unit = h.map(|h| h.prices.unit()).unwrap_or_else(|| Prices::default().unit());
                let currency = h.and_then(|h| model::usage_currency(&h.usage).or(h.prices.currency.clone())).unwrap_or_else(|| "DKK".into());
                (price_unit, currency)
            };
            account.register(info, has_ws, &price_unit, &currency).await?;
        }
        account.publish(Utc::now()).await;

        let acc = account.clone();
        let poll = tokio::spawn(async move {
            loop {
                tokio::time::sleep(TICK).await;
                acc.tick(Utc::now()).await;
            }
        });
        account.tasks.lock().unwrap().push(poll);
        let live: Vec<String> = viewer.homes.iter().filter(|h| h.realtime).map(|h| h.id.clone()).collect();
        if let (Some(url), false) = (viewer.websocket_url, live.is_empty()) {
            let acc = account.clone();
            account.tasks.lock().unwrap().push(tokio::spawn(async move { acc.live_loop(url, live).await }));
        }
        if let Some(old) = self.accounts.lock().unwrap().insert(entry_id, account) {
            old.stop();
        }
        Ok(())
    }

    async fn unload_entry(&self, _host: &Host, entry_id: &str) -> Result<(), Error> {
        if let Some(a) = self.accounts.lock().unwrap().remove(entry_id) {
            a.stop();
        }
        Ok(())
    }

    async fn diagnostics(&self, _host: &Host, entry: Entry) -> Value {
        let mut d = entry.data.clone();
        if let Value::Object(m) = &mut d {
            m.insert("token".into(), json!("**REDACTED**"));
        }
        let homes: Vec<Value> = self
            .accounts
            .lock()
            .unwrap()
            .get(&entry.entry_id)
            .map(|a| {
                a.homes
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|h| {
                        json!({
                            "id": model::short_id(&h.info.id), "realtime": h.info.realtime,
                            "prices_today": h.prices.today.len(), "prices_tomorrow": h.prices.tomorrow.len(),
                            "currency": h.prices.currency, "usage_hours": h.usage.len()
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        json!({ "entry": d, "homes": homes })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    PriceNow,
    PriceLevel,
    PriceToday,
    PriceTomorrow,
    LastHour,
    UsageToday,
    CostToday,
    LivePower,
    LiveProduction,
    LiveConsumptionToday,
    LiveMeter,
}

impl Kind {
    fn live(self) -> bool {
        matches!(self, Kind::LivePower | Kind::LiveProduction | Kind::LiveConsumptionToday | Kind::LiveMeter)
    }
}

struct Home {
    info: HomeInfo,
    prices: Prices,
    usage: Vec<Usage>,
    live_published: Option<Instant>,
}

struct Account {
    entry_id: String,
    host: Host,
    client: Arc<Client>,
    quarter: bool,
    homes: Mutex<Vec<Home>>,
    /// (entity_id, home id, kind)
    entities: Mutex<Vec<(String, String, Kind)>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    paused_until: Mutex<Option<Instant>>,
    prices_fetched: Mutex<Option<DateTime<Utc>>>,
    usage_fetched: Mutex<Option<Instant>>,
}

impl Account {
    fn stop(&self) {
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
    }

    async fn register(&self, info: &HomeInfo, has_ws: bool, price_unit: &str, currency: &str) -> Result<(), Error> {
        let host = &self.host;
        let dev = host
            .register_device(
                &self.entry_id,
                Device {
                    identifiers: vec![(DOMAIN.into(), info.id.clone())],
                    name: info.name.clone(),
                    manufacturer: Some("Tibber".into()),
                    model: Some(if info.realtime { "Hjem med Pulse".into() } else { "Hjem".into() }),
                    ..Default::default()
                },
            )
            .await?;
        let id = &info.id;
        let entity = |key: &str, name: &str, domain: &str| Entity::new(domain, format!("{id}-{key}")).name(name).device(&dev);
        let money = |e: Entity| e.device_class("monetary").unit(price_unit).icon("mdi:cash");
        let levels: Vec<&str> = model::LEVELS.to_vec();
        let mut list: Vec<(Kind, Entity)> = vec![
            (Kind::PriceNow, money(entity("price", "Elpris", "sensor")).state_class("measurement")),
            (Kind::PriceLevel, entity("price_level", "Prisniveau", "sensor").device_class("enum").icon("mdi:gauge").capabilities(json!({ "options": levels }))),
            (Kind::PriceToday, money(entity("price_today", "Gennemsnitspris i dag", "sensor"))),
            (Kind::PriceTomorrow, money(entity("price_tomorrow", "Gennemsnitspris i morgen", "sensor"))),
            (Kind::LastHour, entity("consumption_last_hour", "Forbrug sidste time", "sensor").device_class("energy").unit("kWh")),
            (Kind::UsageToday, entity("consumption_today", "Forbrug i dag", "sensor").device_class("energy").unit("kWh").state_class("total_increasing")),
            (Kind::CostToday, entity("cost_today", "Udgift i dag", "sensor").device_class("monetary").unit(currency).state_class("total")),
        ];
        if info.realtime && has_ws {
            list.push((Kind::LivePower, entity("power", "Effekt", "sensor").device_class("power").unit("W").state_class("measurement")));
            list.push((Kind::LiveProduction, entity("power_production", "Produktion", "sensor").device_class("power").unit("W").state_class("measurement").disabled_by_default()));
            list.push((
                Kind::LiveConsumptionToday,
                entity("accumulated_consumption", "Forbrug i dag (Pulse)", "sensor").device_class("energy").unit("kWh").state_class("total_increasing"),
            ));
            list.push((Kind::LiveMeter, entity("meter_consumption", "Målerstand", "sensor").device_class("energy").unit("kWh").state_class("total_increasing")));
        }
        for (kind, e) in list {
            if let Some(entity_id) = host.add_entity(&self.entry_id, e, false).await? {
                self.entities.lock().unwrap().push((entity_id, id.clone(), kind));
            }
        }
        Ok(())
    }

    fn paused(&self) -> bool {
        self.paused_until.lock().unwrap().is_some_and(|t| Instant::now() < t)
    }

    async fn fetch_prices(&self, now: DateTime<Utc>) -> Result<(), ApiError> {
        let data = self.client.prices(self.quarter).await?;
        let mut homes = self.homes.lock().unwrap();
        for h in homes.iter_mut() {
            if let Some(v) = model::home(&data, &h.info.id) {
                h.prices = model::parse_prices(v);
            }
        }
        *self.prices_fetched.lock().unwrap() = Some(now);
        Ok(())
    }

    async fn fetch_usage(&self) -> Result<(), ApiError> {
        *self.usage_fetched.lock().unwrap() = Some(Instant::now());
        let data = self.client.consumption(USAGE_HOURS).await?;
        let mut homes = self.homes.lock().unwrap();
        for h in homes.iter_mut() {
            if let Some(v) = model::home(&data, &h.info.id) {
                h.usage = model::parse_usage(v);
            }
        }
        Ok(())
    }

    /// Fetch what is due and update the price and consumption sensors.
    async fn tick(&self, now: DateTime<Utc>) {
        let due = {
            let mut homes = self.homes.lock().unwrap();
            let last = *self.prices_fetched.lock().unwrap();
            homes.iter_mut().fold(false, |due, h| {
                h.prices.roll(now);
                due | h.prices.fetch_due(now, last)
            })
        };
        if !self.paused() {
            let mut ok = true;
            if due {
                if let Err(e) = self.fetch_prices(now).await {
                    ok = false;
                    self.report(&e).await;
                }
            }
            let usage_due = self.usage_fetched.lock().unwrap().is_none_or(|t| t.elapsed() >= USAGE_POLL);
            if usage_due && !self.paused() {
                match self.fetch_usage().await {
                    Ok(()) => {}
                    Err(e) => {
                        ok = false;
                        self.report(&e).await;
                    }
                }
            }
            if ok && (due || usage_due) {
                let _ = self.host.delete_issue(&self.entry_id, "auth").await;
            }
        }
        self.publish(now).await;
    }

    /// Push the price and consumption states of every home.
    async fn publish(&self, now: DateTime<Utc>) {
        let updates: Vec<(String, Option<StateUpdate>)> = {
            let homes = self.homes.lock().unwrap();
            self.entities
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, _, k)| !k.live())
                .filter_map(|(entity_id, home, kind)| {
                    let h = homes.iter().find(|h| &h.info.id == home)?;
                    let value = match kind {
                        Kind::PriceNow => model::price_now(&h.prices, now),
                        Kind::PriceLevel => model::price_level(&h.prices, now),
                        Kind::PriceToday => model::day_price(&h.prices.today),
                        Kind::PriceTomorrow => model::day_price(&h.prices.tomorrow),
                        Kind::LastHour => model::last_hour(&h.usage),
                        Kind::UsageToday => model::usage_today(&h.usage, now).map(|(kwh, _)| (kwh.to_string(), Map::new())),
                        Kind::CostToday => model::usage_today(&h.usage, now).and_then(|(_, c)| c).map(|c| (c.to_string(), Map::new())),
                        _ => return None,
                    };
                    Some((entity_id.clone(), value))
                })
                .collect()
        };
        self.push(updates).await;
    }

    async fn push(&self, updates: Vec<(String, Option<StateUpdate>)>) {
        for (entity_id, value) in updates {
            let res = match value {
                Some((state, attrs)) => self.host.set_state(&entity_id, state, attrs).await,
                None => self.host.set_state(&entity_id, "unknown", Map::new()).await,
            };
            if let Err(e) = res {
                warn!("kunne ikke opdatere {entity_id}: {e}");
            }
        }
    }

    /// A live measurement for a home. At most one update per
    /// [`LIVE_MIN_INTERVAL`], since Pulse sends one every few seconds.
    async fn live_data(&self, home: &str, m: &Value) {
        {
            let mut homes = self.homes.lock().unwrap();
            let Some(h) = homes.iter_mut().find(|h| h.info.id == home) else { return };
            if h.live_published.is_some_and(|t| t.elapsed() < LIVE_MIN_INTERVAL) {
                return;
            }
            h.live_published = Some(Instant::now());
        }
        let updates: Vec<(String, Option<StateUpdate>)> = self
            .entities
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, h, k)| h == home && k.live())
            .map(|(entity_id, _, kind)| {
                let value = match kind {
                    Kind::LivePower => model::live_power(m),
                    Kind::LiveProduction => model::live_number(m, "powerProduction", 0),
                    Kind::LiveConsumptionToday => model::live_number(m, "accumulatedConsumption", 3),
                    _ => model::live_number(m, "lastMeterConsumption", 3),
                };
                (entity_id.clone(), value)
            })
            // keep the last value when a field is missing from one message
            .filter(|(_, v)| v.is_some())
            .collect();
        self.push(updates).await;
    }

    async fn live_unavailable(&self) {
        let ids: Vec<String> = self.entities.lock().unwrap().iter().filter(|(_, _, k)| k.live()).map(|(e, _, _)| e.clone()).collect();
        for id in ids {
            let _ = self.host.set_unavailable(&id).await;
        }
        for h in self.homes.lock().unwrap().iter_mut() {
            h.live_published = None;
        }
    }

    /// Keep the websocket open, reconnecting with a growing pause.
    async fn live_loop(self: Arc<Self>, url: String, homes: Vec<String>) {
        let mut backoff = Duration::from_secs(5);
        loop {
            let started = Instant::now();
            if let Err(e) = self.live_session(&url, &homes).await {
                warn!("Tibber Pulse: {e}");
            }
            self.live_unavailable().await;
            if started.elapsed() > Duration::from_secs(120) {
                backoff = Duration::from_secs(5);
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(LIVE_MAX_BACKOFF);
        }
    }

    /// One websocket connection, until it fails or goes quiet.
    async fn live_session(&self, url: &str, homes: &[String]) -> Result<(), String> {
        let mut req = url.into_client_request().map_err(|e| e.to_string())?;
        req.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_static(ws::PROTOCOL));
        req.headers_mut().insert("User-Agent", HeaderValue::from_static(api::USER_AGENT));
        let (stream, _) = tokio::time::timeout(Duration::from_secs(30), tokio_tungstenite::connect_async(req))
            .await
            .map_err(|_| "ingen forbindelse".to_string())?
            .map_err(|e| e.to_string())?;
        let (mut tx, mut rx) = stream.split();
        tx.send(Message::Text(ws::init(self.client.token()))).await.map_err(|e| e.to_string())?;
        let mut subscribed = false;
        loop {
            let msg = match tokio::time::timeout(LIVE_SILENCE, rx.next()).await {
                Err(_) => return Err("ingen data i 90 sekunder".into()),
                Ok(None) => return Err("forbindelsen blev lukket".into()),
                Ok(Some(Err(e))) => return Err(e.to_string()),
                Ok(Some(Ok(m))) => m,
            };
            let text = match msg {
                Message::Text(t) => t,
                Message::Ping(p) => {
                    let _ = tx.send(Message::Pong(p)).await;
                    continue;
                }
                Message::Close(f) => return Err(format!("lukket: {}", f.map(|f| f.reason.to_string()).unwrap_or_default())),
                _ => continue,
            };
            match ws::parse(&text) {
                ws::Incoming::Ack if !subscribed => {
                    subscribed = true;
                    for h in homes {
                        tx.send(Message::Text(ws::subscribe(h))).await.map_err(|e| e.to_string())?;
                    }
                    info!("Tibber Pulse forbundet for {} hjem", homes.len());
                }
                ws::Incoming::Ping => {
                    let _ = tx.send(Message::Text(ws::pong())).await;
                }
                ws::Incoming::Data(home, m) => self.live_data(&home, &m).await,
                ws::Incoming::Error(e) => return Err(e),
                _ => {}
            }
        }
    }

    async fn report(&self, e: &ApiError) {
        match e {
            ApiError::Auth => {
                let r = self
                    .host
                    .create_issue(
                        &self.entry_id,
                        "auth",
                        "error",
                        "Tibber: adgangsnøglen virker ikke",
                        "Tibber afviste adgangsnøglen. Lav en ny på developer.tibber.com, og godkend integrationen igen med den.",
                    )
                    .await;
                if let Err(e) = r {
                    warn!("kunne ikke oprette problem: {e}");
                }
            }
            ApiError::RateLimited(after) => {
                let pause = after.unwrap_or(RATE_LIMIT_PAUSE);
                warn!("Tibber svarede 429; venter {} minutter", pause.as_secs().div_ceil(60));
                *self.paused_until.lock().unwrap() = Some(Instant::now() + pause);
            }
            ApiError::Other(err) => warn!("{err}"),
        }
    }
}
