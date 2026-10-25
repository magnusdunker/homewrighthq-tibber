//! Tibber's GraphQL API, the one developer.tibber.com describes.
//!
//! Every request carries the personal access token as a bearer token and a
//! User-Agent that names the integration, as Tibber asks.

use std::time::Duration;

use serde_json::{json, Value};

pub const BASE_URL: &str = "https://api.tibber.com/v1-beta/gql";
pub const USER_AGENT: &str = concat!("homewright-tibber/", env!("CARGO_PKG_VERSION"));

/// The account, its homes and the address of the live data websocket.
pub const VIEWER_QUERY: &str = "{ viewer { userId name login websocketSubscriptionUrl homes { id appNickname timeZone \
     address { address1 postalCode city } features { realTimeConsumptionEnabled } currentSubscription { status } } } }";

const PRICE_FIELDS: &str = "total energy tax startsAt level currency";

/// Prices now, today and tomorrow for every home. Without `quarter` the
/// argument is left out, which gives Tibber's default of one price per hour.
pub fn prices_query(quarter: bool) -> String {
    let arg = if quarter { "(resolution: QUARTER_HOURLY)" } else { "" };
    format!(
        "{{ viewer {{ homes {{ id currentSubscription {{ priceInfo{arg} {{ current {{ {PRICE_FIELDS} }} today {{ {PRICE_FIELDS} }} tomorrow {{ {PRICE_FIELDS} }} }} }} }} }} }}"
    )
}

/// The last `hours` hours of consumption for every home.
pub fn consumption_query(hours: u32) -> String {
    format!("{{ viewer {{ homes {{ id consumption(resolution: HOURLY, last: {hours}) {{ nodes {{ from to consumption cost unitPrice currency }} }} }} }} }}")
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    /// The token is wrong or has been revoked.
    Auth,
    /// HTTP 429; the duration is `Retry-After` when Tibber sends it.
    RateLimited(Option<Duration>),
    Other(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Auth => write!(f, "Tibber afviste adgangsnøglen"),
            ApiError::RateLimited(_) => write!(f, "Tibber beder om færre forespørgsler (HTTP 429)"),
            ApiError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        ApiError::Other(e.to_string())
    }
}

pub struct Client {
    http: reqwest::Client,
    url: String,
    token: String,
}

impl Client {
    pub fn new(url: &str, token: &str) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).user_agent(USER_AGENT).build().unwrap_or_default();
        Self { http, url: url.to_string(), token: token.trim().to_string() }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// Run a query and return its `data`.
    pub async fn query(&self, query: &str) -> Result<Value, ApiError> {
        let res = self.http.post(&self.url).bearer_auth(&self.token).json(&json!({ "query": query })).send().await?;
        let status = res.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let after = res.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse::<u64>().ok()).map(Duration::from_secs);
            return Err(ApiError::RateLimited(after));
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ApiError::Auth);
        }
        let text = res.text().await?;
        let body: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) if !status.is_success() => return Err(ApiError::Other(format!("HTTP {status}: {}", text.chars().take(200).collect::<String>()))),
            Err(e) => return Err(ApiError::Other(format!("ugyldigt svar: {e}"))),
        };
        if let Some(errors) = body.get("errors").and_then(Value::as_array).filter(|e| !e.is_empty()) {
            if errors.iter().any(is_auth_error) {
                return Err(ApiError::Auth);
            }
            if body.get("data").is_none_or(Value::is_null) {
                let msg = errors.iter().filter_map(|e| e["message"].as_str()).collect::<Vec<_>>().join("; ");
                return Err(ApiError::Other(format!("Tibber: {msg}")));
            }
        }
        if !status.is_success() {
            return Err(ApiError::Other(format!("HTTP {status}")));
        }
        Ok(body.get("data").cloned().unwrap_or(Value::Null))
    }

    pub async fn viewer(&self) -> Result<Value, ApiError> {
        self.query(VIEWER_QUERY).await
    }

    pub async fn prices(&self, quarter: bool) -> Result<Value, ApiError> {
        self.query(&prices_query(quarter)).await
    }

    pub async fn consumption(&self, hours: u32) -> Result<Value, ApiError> {
        self.query(&consumption_query(hours)).await
    }
}

fn is_auth_error(e: &Value) -> bool {
    let code = e.pointer("/extensions/code").and_then(Value::as_str).unwrap_or_default();
    let msg = e.get("message").and_then(Value::as_str).unwrap_or_default().to_lowercase();
    code == "UNAUTHENTICATED" || msg.contains("invalid token") || msg.contains("context creation failed")
}
