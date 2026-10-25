#![allow(dead_code)]

//! Plays HomewrightHQ over a pipe: answers the integration's requests the way
//! HomewrightHQ does and records devices, entities, states and issues.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use homewright_plugin::Integration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

#[derive(Default)]
pub struct Recorded {
    pub devices: Vec<Value>,
    /// entity_id -> (spec, handler)
    pub entities: HashMap<String, (Value, bool)>,
    pub states: HashMap<String, (String, Value)>,
    pub issues: Vec<String>,
    /// The data of every `entry/update`, newest last.
    pub entry_updates: Vec<Value>,
}

pub struct FakeHost {
    tx: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next: AtomicU64,
    pub rec: Arc<Mutex<Recorded>>,
}

impl FakeHost {
    pub fn start<I: Integration>(integration: I) -> Self {
        let (plugin_io, host_io) = tokio::io::duplex(1 << 20);
        let (pr, pw) = tokio::io::split(plugin_io);
        tokio::spawn(homewright_plugin::serve_io(integration, pr, pw));
        let (hr, mut hw) = tokio::io::split(host_io);

        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if hw.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> = Arc::default();
        let rec: Arc<Mutex<Recorded>> = Arc::default();
        let (p, r, t) = (pending.clone(), rec.clone(), tx.clone());
        tokio::spawn(async move {
            let mut lines = BufReader::new(hr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let msg: Value = serde_json::from_str(&line).unwrap();
                match msg.get("method").and_then(Value::as_str) {
                    Some(method) => {
                        let result = answer(&r, method, &msg["params"]);
                        if let Some(id) = msg.get("id") {
                            let _ = t.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string());
                        }
                    }
                    None => {
                        let id = msg["id"].as_u64().unwrap();
                        if let Some(s) = p.lock().unwrap().remove(&id) {
                            let _ = s.send(msg);
                        }
                    }
                }
            }
        });
        Self { tx, pending, next: AtomicU64::new(1), rec }
    }

    /// Call the integration; `Err` holds the error object.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (s, r) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, s);
        self.tx.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()).unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(20), r).await.expect("no answer").unwrap();
        match reply.get("error") {
            Some(e) if !e.is_null() => Err(e.clone()),
            _ => Ok(reply["result"].clone()),
        }
    }

    pub fn state(&self, entity_id: &str) -> Option<(String, Value)> {
        self.rec.lock().unwrap().states.get(entity_id).cloned()
    }

    /// The entity_id of the entity with this unique_id.
    pub fn entity(&self, unique_id: &str) -> String {
        let rec = self.rec.lock().unwrap();
        rec.entities
            .iter()
            .find(|(_, (spec, _))| spec["unique_id"] == unique_id)
            .map(|(k, _)| k.clone())
            .unwrap_or_else(|| panic!("{unique_id} mangler i {:?}", rec.entities.keys()))
    }

    pub fn entity_ending(&self, suffix: &str) -> String {
        let rec = self.rec.lock().unwrap();
        rec.entities.keys().find(|k| k.ends_with(suffix)).cloned().unwrap_or_else(|| panic!("{suffix} mangler i {:?}", rec.entities.keys()))
    }
}

/// Entity ids from the unique_id, so they never collide in tests.
fn slug(s: &str) -> String {
    s.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

fn answer(rec: &Mutex<Recorded>, method: &str, p: &Value) -> Value {
    let mut rec = rec.lock().unwrap();
    match method {
        "device/register" => {
            rec.devices.push(p["device"].clone());
            json!({ "id": format!("dev{}", rec.devices.len()), "name": p["device"]["name"] })
        }
        "entity/add" => {
            let e = &p["entity"];
            let id = format!("{}.{}", e["domain"].as_str().unwrap(), slug(e["unique_id"].as_str().unwrap()));
            rec.entities.insert(id.clone(), (e.clone(), p["handler"].as_bool().unwrap_or(false)));
            json!({ "entity_id": id })
        }
        "state/set" => {
            let id = p["entity_id"].as_str().unwrap().to_string();
            rec.states.insert(id, (p["state"].as_str().unwrap_or("").to_string(), p["attributes"].clone()));
            Value::Null
        }
        "state/unavailable" => {
            let id = p["entity_id"].as_str().unwrap().to_string();
            rec.states.insert(id, ("unavailable".into(), Value::Null));
            Value::Null
        }
        "entry/update" => {
            rec.entry_updates.push(p["data"].clone());
            json!({ "entry_id": p["entry_id"], "data": p["data"] })
        }
        "issue/delete" => {
            let id = p["issue_id"].as_str().unwrap_or("");
            rec.issues.retain(|i| i != id);
            Value::Null
        }
        "issue/create" => {
            rec.issues.push(p["issue_id"].as_str().unwrap_or("").to_string());
            Value::Null
        }
        _ => Value::Null,
    }
}
