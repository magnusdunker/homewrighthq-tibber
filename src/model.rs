//! Tibber's data and how it becomes states and attributes.

use chrono::{DateTime, Duration, FixedOffset, Utc};
use serde_json::{json, Map, Value};

pub type StateUpdate = (String, Map<String, Value>);

/// One home on the account.
#[derive(Clone, Debug, PartialEq)]
pub struct HomeInfo {
    pub id: String,
    pub name: String,
    pub address: Option<String>,
    /// The home has a Tibber Pulse or Watty that sends live data.
    pub realtime: bool,
}

/// The account from the viewer query.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Viewer {
    pub user_id: String,
    pub name: String,
    pub websocket_url: Option<String>,
    pub homes: Vec<HomeInfo>,
}

pub fn parse_viewer(data: &Value) -> Viewer {
    let v = &data["viewer"];
    let homes = v["homes"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|h| {
                    let id = h["id"].as_str()?.to_string();
                    let address = h["address"]["address1"].as_str().map(String::from);
                    let name = h["appNickname"].as_str().filter(|s| !s.trim().is_empty()).map(String::from).or_else(|| address.clone()).unwrap_or_else(|| format!("Hjem {}", short_id(&id)));
                    Some(HomeInfo { id, name, address, realtime: h["features"]["realTimeConsumptionEnabled"].as_bool().unwrap_or(false) })
                })
                .collect()
        })
        .unwrap_or_default();
    Viewer {
        user_id: v["userId"].as_str().unwrap_or_default().to_string(),
        name: v["name"].as_str().or(v["login"].as_str()).unwrap_or_default().to_string(),
        websocket_url: v["websocketSubscriptionUrl"].as_str().filter(|s| !s.is_empty()).map(String::from),
        homes,
    }
}

/// The first part of a home id, for names and statistic ids.
pub fn short_id(id: &str) -> String {
    id.split('-').next().unwrap_or(id).to_string()
}

/// The home with this id in a `viewer { homes { id ... } }` answer.
pub fn home<'a>(data: &'a Value, id: &str) -> Option<&'a Value> {
    data["viewer"]["homes"].as_array()?.iter().find(|h| h["id"] == id)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Slot {
    pub start: DateTime<FixedOffset>,
    pub end: DateTime<FixedOffset>,
    /// Price including taxes, in the currency per kWh.
    pub total: f64,
    pub energy: Option<f64>,
    pub tax: Option<f64>,
    /// Tibber's level: `VERY_CHEAP`, `CHEAP`, `NORMAL`, `EXPENSIVE`, `VERY_EXPENSIVE`.
    pub level: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Prices {
    pub today: Vec<Slot>,
    pub tomorrow: Vec<Slot>,
    pub currency: Option<String>,
}

pub const LEVELS: [&str; 5] = ["very_cheap", "cheap", "normal", "expensive", "very_expensive"];

fn parse_slots(v: &Value) -> Vec<Slot> {
    let mut raw: Vec<(DateTime<FixedOffset>, &Value)> = v
        .as_array()
        .map(|a| a.iter().filter_map(|p| Some((DateTime::parse_from_rfc3339(p["startsAt"].as_str()?).ok()?, p))).filter(|(_, p)| p["total"].is_number()).collect())
        .unwrap_or_default();
    raw.sort_by_key(|(t, _)| *t);
    // the length of one slot: the gap between the first two, else an hour
    let step = match raw.as_slice() {
        [a, b, ..] if b.0 > a.0 => b.0 - a.0,
        _ => Duration::hours(1),
    };
    (0..raw.len())
        .map(|i| {
            let (start, p) = raw[i];
            let end = raw.get(i + 1).map(|n| n.0).filter(|n| *n - start <= step).unwrap_or(start + step);
            Slot {
                start,
                end,
                total: p["total"].as_f64().unwrap_or_default(),
                energy: p["energy"].as_f64(),
                tax: p["tax"].as_f64(),
                level: p["level"].as_str().map(String::from),
            }
        })
        .collect()
}

/// `priceInfo` of one home from the prices query.
pub fn parse_prices(home: &Value) -> Prices {
    let info = &home["currentSubscription"]["priceInfo"];
    let currency = [&info["current"], &info["today"][0], &info["tomorrow"][0]].iter().find_map(|p| p["currency"].as_str()).map(String::from);
    Prices { today: parse_slots(&info["today"]), tomorrow: parse_slots(&info["tomorrow"]), currency }
}

fn round(v: f64, digits: i32) -> f64 {
    let f = 10f64.powi(digits);
    (v * f).round() / f
}

fn slot_json(s: &Slot) -> Value {
    json!({ "start": s.start.to_rfc3339(), "end": s.end.to_rfc3339(), "price": round(s.total, 4) })
}

fn stats(slots: &[Slot]) -> Option<(f64, f64, f64)> {
    if slots.is_empty() {
        return None;
    }
    let prices: Vec<f64> = slots.iter().map(|s| s.total).collect();
    let min = prices.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = prices.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    Some((min, max, prices.iter().sum::<f64>() / prices.len() as f64))
}

impl Prices {
    pub fn unit(&self) -> String {
        format!("{}/kWh", self.currency.as_deref().unwrap_or("DKK"))
    }

    pub fn current(&self, now: DateTime<Utc>) -> Option<&Slot> {
        self.today.iter().chain(&self.tomorrow).find(|s| s.start <= now && now < s.end)
    }

    /// When a new fetch is due: at the start, when today has run out, every
    /// 15 minutes from 13:00 until tomorrow's prices are there, and at the
    /// latest every six hours.
    pub fn fetch_due(&self, now: DateTime<Utc>, last: Option<DateTime<Utc>>) -> bool {
        let Some(last) = last else { return true };
        let since = now - last;
        let Some(first) = self.today.first() else { return since >= Duration::minutes(10) };
        if self.today.last().is_some_and(|s| now >= s.end) {
            return since >= Duration::minutes(5);
        }
        if self.tomorrow.is_empty() && now >= first.start + Duration::hours(13) {
            return since >= Duration::minutes(15);
        }
        since >= Duration::hours(6)
    }

    /// When today has run out and tomorrow is known, tomorrow becomes today.
    pub fn roll(&mut self, now: DateTime<Utc>) {
        if !self.tomorrow.is_empty() && self.today.last().is_none_or(|s| now >= s.end) {
            self.today = std::mem::take(&mut self.tomorrow);
        }
    }
}

/// The price sensor: the price now, with today's and tomorrow's prices in the
/// shape HomewrightHQ's Energy page reads.
pub fn price_now(p: &Prices, now: DateTime<Utc>) -> Option<StateUpdate> {
    let cur = p.current(now)?;
    let mut a = Map::new();
    a.insert("today".into(), Value::Array(p.today.iter().map(slot_json).collect()));
    a.insert("tomorrow".into(), Value::Array(p.tomorrow.iter().map(slot_json).collect()));
    a.insert("tomorrow_valid".into(), json!(!p.tomorrow.is_empty()));
    a.insert("energy".into(), json!(cur.energy.map(|v| round(v, 4))));
    a.insert("tax".into(), json!(cur.tax.map(|v| round(v, 4))));
    a.insert("level".into(), json!(cur.level.as_deref().map(str::to_lowercase)));
    a.insert("starts_at".into(), json!(cur.start.to_rfc3339()));
    if let Some((min, max, avg)) = stats(&p.today) {
        a.insert("min_price".into(), json!(round(min, 4)));
        a.insert("max_price".into(), json!(round(max, 4)));
        a.insert("avg_price".into(), json!(round(avg, 4)));
    }
    Some((round(cur.total, 4).to_string(), a))
}

pub fn price_level(p: &Prices, now: DateTime<Utc>) -> Option<StateUpdate> {
    let level = p.current(now)?.level.as_deref()?.to_lowercase();
    LEVELS.contains(&level.as_str()).then(|| (level, Map::new()))
}

/// A day's average price, with the day's prices as `prices`.
pub fn day_price(slots: &[Slot]) -> Option<StateUpdate> {
    let (min, max, avg) = stats(slots)?;
    let mut a = Map::new();
    a.insert("prices".into(), Value::Array(slots.iter().map(slot_json).collect()));
    a.insert("min_price".into(), json!(round(min, 4)));
    a.insert("max_price".into(), json!(round(max, 4)));
    let cheapest = slots.iter().min_by(|a, b| a.total.total_cmp(&b.total)).map(|s| s.start.to_rfc3339());
    a.insert("cheapest_start".into(), json!(cheapest));
    Some((round(avg, 4).to_string(), a))
}

/// One hour of consumption.
#[derive(Clone, Debug, PartialEq)]
pub struct Usage {
    pub from: DateTime<FixedOffset>,
    pub to: DateTime<FixedOffset>,
    pub kwh: f64,
    pub cost: Option<f64>,
    pub currency: Option<String>,
}

/// `consumption.nodes` of one home. Hours Tibber has no reading for yet are
/// left out.
pub fn parse_usage(home: &Value) -> Vec<Usage> {
    home["consumption"]["nodes"]
        .as_array()
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|n| {
                    Some(Usage {
                        from: DateTime::parse_from_rfc3339(n["from"].as_str()?).ok()?,
                        to: DateTime::parse_from_rfc3339(n["to"].as_str()?).ok()?,
                        kwh: n["consumption"].as_f64()?,
                        cost: n["cost"].as_f64(),
                        currency: n["currency"].as_str().map(String::from),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The newest hour with a reading.
pub fn last_hour(usage: &[Usage]) -> Option<StateUpdate> {
    let u = usage.iter().max_by_key(|u| u.from)?;
    let mut a = Map::new();
    a.insert("from".into(), json!(u.from.to_rfc3339()));
    a.insert("to".into(), json!(u.to.to_rfc3339()));
    a.insert("cost".into(), json!(u.cost.map(|c| round(c, 2))));
    a.insert("currency".into(), json!(u.currency));
    Some((round(u.kwh, 3).to_string(), a))
}

/// Consumption and cost since midnight, counted in the time zone Tibber uses
/// for the home. `None` before the first hour of the day has a reading.
pub fn usage_today(usage: &[Usage], now: DateTime<Utc>) -> Option<(f64, Option<f64>)> {
    let newest = usage.iter().max_by_key(|u| u.from)?;
    let today = now.with_timezone(newest.from.offset()).date_naive();
    let hours: Vec<&Usage> = usage.iter().filter(|u| u.from.date_naive() == today).collect();
    if hours.is_empty() {
        return None;
    }
    let kwh = hours.iter().map(|u| u.kwh).sum::<f64>();
    let cost = hours.iter().map(|u| u.cost).sum::<Option<f64>>();
    Some((round(kwh, 3), cost.map(|c| round(c, 2))))
}

/// The currency the consumption is priced in.
pub fn usage_currency(usage: &[Usage]) -> Option<String> {
    usage.iter().find_map(|u| u.currency.clone())
}

/// What a live measurement from Tibber Pulse turns into.
pub fn live_power(m: &Value) -> Option<StateUpdate> {
    let p = m["power"].as_f64()?;
    let mut a = Map::new();
    for (key, attr) in [
        ("minPower", "min_power"),
        ("maxPower", "max_power"),
        ("averagePower", "average_power"),
        ("voltagePhase1", "voltage_phase1"),
        ("voltagePhase2", "voltage_phase2"),
        ("voltagePhase3", "voltage_phase3"),
        ("currentL1", "current_l1"),
        ("currentL2", "current_l2"),
        ("currentL3", "current_l3"),
        ("signalStrength", "signal_strength"),
        ("timestamp", "timestamp"),
    ] {
        if !m[key].is_null() {
            a.insert(attr.into(), m[key].clone());
        }
    }
    Some((p.to_string(), a))
}

pub fn live_number(m: &Value, key: &str, digits: i32) -> Option<StateUpdate> {
    m[key].as_f64().map(|v| (round(v, digits).to_string(), Map::new()))
}

/// The fields asked for in the live subscription.
pub const LIVE_FIELDS: &str = "timestamp power powerProduction accumulatedConsumption accumulatedProduction accumulatedCost currency \
     minPower maxPower averagePower lastMeterConsumption lastMeterProduction voltagePhase1 voltagePhase2 voltagePhase3 \
     currentL1 currentL2 currentL3 signalStrength";

/// Messages of the graphql-transport-ws protocol Tibber's websocket speaks.
pub mod ws {
    use super::LIVE_FIELDS;
    use serde_json::{json, Value};

    pub const PROTOCOL: &str = "graphql-transport-ws";

    pub fn init(token: &str) -> String {
        json!({ "type": "connection_init", "payload": { "token": token } }).to_string()
    }

    /// Subscribe to one home's live data; the message id is the home id.
    pub fn subscribe(home_id: &str) -> String {
        let query = format!("subscription {{ liveMeasurement(homeId: \"{home_id}\") {{ {LIVE_FIELDS} }} }}");
        json!({ "id": home_id, "type": "subscribe", "payload": { "query": query } }).to_string()
    }

    pub fn pong() -> String {
        json!({ "type": "pong" }).to_string()
    }

    #[derive(Debug, PartialEq)]
    pub enum Incoming {
        Ack,
        Ping,
        /// A measurement for the home with this id.
        Data(String, Value),
        /// The server ended or refused a subscription, or the connection.
        Error(String),
        Other,
    }

    pub fn parse(text: &str) -> Incoming {
        let Ok(v) = serde_json::from_str::<Value>(text) else { return Incoming::Other };
        let id = v["id"].as_str().unwrap_or_default().to_string();
        match v["type"].as_str().unwrap_or_default() {
            "connection_ack" => Incoming::Ack,
            "ping" => Incoming::Ping,
            "next" => match v.pointer("/payload/data/liveMeasurement") {
                Some(m) if m.is_object() => Incoming::Data(id, m.clone()),
                _ => match v.pointer("/payload/errors") {
                    Some(e) => Incoming::Error(format!("{id}: {e}")),
                    None => Incoming::Other,
                },
            },
            "error" => Incoming::Error(format!("{id}: {}", v["payload"])),
            "complete" => Incoming::Error(format!("{id}: abonnementet blev afsluttet")),
            _ => Incoming::Other,
        }
    }
}
