use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One line on the bus (newline-delimited JSON).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Msg {
    /// First message of every connection: who am I, what do I want to hear.
    Hello {
        role: String,
        pid: u32,
        unit_version: String,
        proto: (u32, u32),
        #[serde(default)]
        subscribe: Vec<String>,
    },
    Welcome {
        core_version: String,
    },
    /// Request to a role. Core rewrites `id` when forwarding and fills in `from`.
    Request {
        id: u64,
        to: String,
        #[serde(default)]
        from: String,
        method: String,
        #[serde(default)]
        params: Value,
    },
    Response {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ok: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        err: Option<String>,
    },
    /// Broadcast to every connection whose subscriptions match the topic prefix.
    Event {
        topic: String,
        #[serde(default)]
        data: Value,
    },
}

impl Msg {
    pub fn line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("bus message serializes");
        s.push('\n');
        s
    }

    pub fn response(id: u64, result: Result<Value, String>) -> Msg {
        match result {
            Ok(v) => Msg::Response { id, ok: Some(v), err: None },
            Err(e) => Msg::Response { id, ok: None, err: Some(e) },
        }
    }
}

/// Does a subscription list match a topic? An empty prefix or `*` matches everything.
pub fn subscribed(subs: &[String], topic: &str) -> bool {
    subs.iter().any(|s| s.is_empty() || s == "*" || topic.starts_with(s.as_str()))
}
