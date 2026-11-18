//! The bus hub: accepts connections, routes requests by role, queues requests for roles that
//! are reloading, broadcasts events by subscription.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use interprocess::local_socket::tokio::{prelude::*, Stream};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};

use tk_bus::proto::{subscribed, Msg};

pub enum Origin {
    Client { conn: u64, id: u64 },
    Core(oneshot::Sender<Result<Value, String>>),
}

impl Origin {
    fn deliver(self, st: &St, result: Result<Value, String>) {
        match self {
            Origin::Client { conn, id } => {
                if let Some(c) = st.conns.get(&conn) {
                    let _ = c.tx.send(Msg::response(id, result).line());
                }
            }
            Origin::Core(tx) => {
                let _ = tx.send(result);
            }
        }
    }
}

pub struct Conn {
    pub role: String,
    pub pid: u32,
    pub unit_version: String,
    pub proto: (u32, u32),
    subs: Vec<String>,
    tx: mpsc::UnboundedSender<String>,
}

struct Queued {
    origin: Origin,
    from: String,
    method: String,
    params: Value,
    at: Instant,
}

#[derive(Default)]
pub struct St {
    pub conns: HashMap<u64, Conn>,
    pub roles: HashMap<String, u64>,
    queued: HashMap<String, Vec<Queued>>,
    forwards: HashMap<(u64, u64), (String, Origin)>,
    pub draining: HashSet<String>,
    pub inflight: HashMap<String, usize>,
}

/// Presence of a layer role: `Some(pid)` while connected.
pub type Presence = watch::Sender<Option<u32>>;

pub struct Router {
    pub st: Mutex<St>,
    next_conn: AtomicU64,
    next_fwd: AtomicU64,
    pub presence: HashMap<String, Presence>,
    pub core_handler: tk_bus::Handler,
}

impl Router {
    pub fn new(roles: &[&str], core_handler: tk_bus::Handler) -> Arc<Router> {
        let presence = roles.iter().map(|r| (r.to_string(), watch::channel(None).0)).collect();
        Arc::new(Router {
            st: Mutex::new(St::default()),
            next_conn: AtomicU64::new(1),
            next_fwd: AtomicU64::new(1),
            presence,
            core_handler,
        })
    }

    /// Call a role from Core itself. Queued while the role is absent or draining.
    pub async fn call(&self, to: &str, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let (tx, rx) = oneshot::channel();
        self.route(Origin::Core(tx), "core", to, method, params);
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("dropped".into()),
            Err(_) => Err(format!("timeout calling {to}.{method}")),
        }
    }

    pub fn route(&self, origin: Origin, from: &str, to: &str, method: &str, params: Value) {
        let mut st = self.st.lock().unwrap();
        let target = st.roles.get(to).copied();
        match target {
            Some(conn) if !st.draining.contains(to) => self.forward(&mut st, conn, origin, from, to, method, params),
            _ => {
                tracing::debug!("queueing {from} -> {to}.{method}");
                st.queued.entry(to.to_string()).or_default().push(Queued {
                    origin,
                    from: from.to_string(),
                    method: method.to_string(),
                    params,
                    at: Instant::now(),
                });
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn forward(&self, st: &mut St, conn: u64, origin: Origin, from: &str, to: &str, method: &str, params: Value) {
        let fid = self.next_fwd.fetch_add(1, Ordering::Relaxed);
        let line = Msg::Request { id: fid, to: to.to_string(), from: from.to_string(), method: method.to_string(), params }.line();
        if let Some(c) = st.conns.get(&conn) {
            let _ = c.tx.send(line);
            st.forwards.insert((conn, fid), (to.to_string(), origin));
            *st.inflight.entry(to.to_string()).or_default() += 1;
        } else {
            origin.deliver(st, Err(format!("{to} vanished")));
        }
    }

    /// Release queued requests for a role that is (again) available.
    pub fn flush(&self, role: &str) {
        let mut st = self.st.lock().unwrap();
        if st.draining.contains(role) {
            return;
        }
        let Some(conn) = st.roles.get(role).copied() else { return };
        let queued = st.queued.remove(role).unwrap_or_default();
        for q in queued {
            self.forward(&mut st, conn, q.origin, &q.from, role, &q.method, q.params);
        }
    }

    /// Fail queued requests older than `max_age`.
    pub fn expire(&self, max_age: Duration) {
        let mut st = self.st.lock().unwrap();
        let mut expired = vec![];
        for (role, list) in st.queued.iter_mut() {
            let (old, keep): (Vec<_>, Vec<_>) = list.drain(..).partition(|q| q.at.elapsed() > max_age);
            *list = keep;
            for q in old {
                expired.push((role.clone(), q));
            }
        }
        for (role, q) in expired {
            q.origin.deliver(&st, Err(format!("{role} not available")));
        }
    }

    pub fn inflight(&self, role: &str) -> usize {
        self.st.lock().unwrap().inflight.get(role).copied().unwrap_or(0)
    }

    pub fn emit(&self, topic: &str, data: Value) {
        let st = self.st.lock().unwrap();
        let line = Msg::Event { topic: topic.to_string(), data }.line();
        for c in st.conns.values() {
            if subscribed(&c.subs, topic) {
                let _ = c.tx.send(line.clone());
            }
        }
    }

    pub fn connected_units(&self) -> Vec<Value> {
        let st = self.st.lock().unwrap();
        st.conns
            .values()
            .map(|c| serde_json::json!({ "role": c.role, "pid": c.pid, "version": c.unit_version, "proto": [c.proto.0, c.proto.1] }))
            .collect()
    }

    pub fn proto_of(&self, role: &str) -> Option<(u32, u32)> {
        let st = self.st.lock().unwrap();
        st.roles.get(role).and_then(|c| st.conns.get(c)).map(|c| c.proto)
    }

    pub async fn serve_conn(self: Arc<Self>, stream: Stream) {
        let conn_id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        #[allow(deprecated)]
        let (r, mut w) = stream.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let writer = tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if w.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let mut lines = BufReader::new(r).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            match serde_json::from_str::<Msg>(&line) {
                Ok(msg) => self.on_msg(conn_id, &tx, msg),
                Err(e) => tracing::warn!("conn {conn_id}: bad line: {e}"),
            }
        }
        self.on_disconnect(conn_id);
        writer.abort();
    }

    fn on_msg(self: &Arc<Self>, conn_id: u64, tx: &mpsc::UnboundedSender<String>, msg: Msg) {
        match msg {
            Msg::Hello { role, pid, unit_version, proto, subscribe } => {
                tracing::info!("hello from {role} pid={pid} version={unit_version}");
                {
                    let mut st = self.st.lock().unwrap();
                    st.conns.insert(
                        conn_id,
                        Conn { role: role.clone(), pid, unit_version, proto, subs: subscribe, tx: tx.clone() },
                    );
                    st.roles.insert(role.clone(), conn_id);
                }
                let _ = tx.send(Msg::Welcome { core_version: env!("CARGO_PKG_VERSION").into() }.line());
                if let Some(p) = self.presence.get(&role) {
                    p.send_replace(Some(pid));
                }
                self.flush(&role);
                self.emit("layer.up", serde_json::json!({ "role": role }));
            }
            Msg::Request { id, to, method, params, .. } => {
                let from = self.role_of(conn_id);
                if to == "core" {
                    let fut = (self.core_handler)(tk_bus::Incoming { from, method, params });
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let res = fut.await;
                        let _ = tx.send(Msg::response(id, res).line());
                    });
                } else {
                    self.route(Origin::Client { conn: conn_id, id }, &from, &to, &method, params);
                }
            }
            Msg::Response { id, ok, err } => {
                let mut st = self.st.lock().unwrap();
                if let Some((role, origin)) = st.forwards.remove(&(conn_id, id)) {
                    if let Some(n) = st.inflight.get_mut(&role) {
                        *n = n.saturating_sub(1);
                    }
                    origin.deliver(&st, match err {
                        Some(e) => Err(e),
                        None => Ok(ok.unwrap_or(Value::Null)),
                    });
                }
            }
            Msg::Event { topic, data } => {
                let st = self.st.lock().unwrap();
                let line = Msg::Event { topic: topic.clone(), data }.line();
                for (cid, c) in st.conns.iter() {
                    if *cid != conn_id && subscribed(&c.subs, &topic) {
                        let _ = c.tx.send(line.clone());
                    }
                }
            }
            Msg::Welcome { .. } => {}
        }
    }

    fn role_of(&self, conn_id: u64) -> String {
        self.st.lock().unwrap().conns.get(&conn_id).map(|c| c.role.clone()).unwrap_or_default()
    }

    fn on_disconnect(&self, conn_id: u64) {
        let mut st = self.st.lock().unwrap();
        let Some(conn) = st.conns.remove(&conn_id) else { return };
        tracing::info!("{} disconnected", conn.role);
        if st.roles.get(&conn.role) == Some(&conn_id) {
            st.roles.remove(&conn.role);
            if let Some(p) = self.presence.get(&conn.role) {
                p.send_replace(None);
            }
        }
        let keys: Vec<_> = st.forwards.keys().filter(|(c, _)| *c == conn_id).copied().collect();
        for k in keys {
            if let Some((role, origin)) = st.forwards.remove(&k) {
                if let Some(n) = st.inflight.get_mut(&role) {
                    *n = n.saturating_sub(1);
                }
                origin.deliver(&st, Err(format!("{role} disconnected before answering")));
            }
        }
        drop(st);
        self.emit("layer.down", serde_json::json!({ "role": conn.role }));
    }
}
