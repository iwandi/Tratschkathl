//! Bus client: connects to Core, reconnects forever, re-sends unanswered requests after a
//! reconnect (so calls survive a Core reload), dispatches incoming requests and events.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use interprocess::local_socket::tokio::{prelude::*, Stream};
use interprocess::local_socket::GenericNamespaced;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};

use crate::paths;
use crate::proto::Msg;

pub struct Incoming {
    pub from: String,
    pub method: String,
    pub params: Value,
}

pub type HandlerFut = Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>;
pub type Handler = Arc<dyn Fn(Incoming) -> HandlerFut + Send + Sync>;

/// Wrap an async closure as a [`Handler`].
pub fn handler<F, Fut>(f: F) -> Handler
where
    F: Fn(Incoming) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, String>> + Send + 'static,
{
    Arc::new(move |inc| Box::pin(f(inc)))
}

struct Pending {
    line: String,
    tx: oneshot::Sender<Result<Value, String>>,
}

struct ClientState {
    out: Option<mpsc::UnboundedSender<String>>,
    pending: HashMap<u64, Pending>,
}

struct Inner {
    role: String,
    next_id: AtomicU64,
    state: Mutex<ClientState>,
    connected: watch::Sender<bool>,
}

#[derive(Clone)]
pub struct Bus {
    inner: Arc<Inner>,
}

pub type Events = mpsc::UnboundedReceiver<(String, Value)>;

impl Bus {
    /// Start a client. Returns at once; the connection is made (and remade) in the background.
    /// Must be called inside a Tokio runtime.
    pub fn connect(
        role: impl Into<String>,
        unit_version: &str,
        subscribe: Vec<String>,
        handler: Option<Handler>,
    ) -> (Bus, Events) {
        let (connected, _) = watch::channel(false);
        let inner = Arc::new(Inner {
            role: role.into(),
            next_id: AtomicU64::new(1),
            state: Mutex::new(ClientState { out: None, pending: HashMap::new() }),
            connected,
        });
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let bus = Bus { inner };
        let hello = Msg::Hello {
            role: bus.inner.role.clone(),
            pid: std::process::id(),
            unit_version: unit_version.to_string(),
            proto: crate::PROTO,
            subscribe,
        }
        .line();
        tokio::spawn(run(bus.clone(), hello, handler, ev_tx));
        (bus, ev_rx)
    }

    pub fn role(&self) -> &str {
        &self.inner.role
    }

    pub fn is_connected(&self) -> bool {
        *self.inner.connected.borrow()
    }

    pub async fn wait_connected(&self) {
        let mut rx = self.inner.connected.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Watch connection state (true = connected).
    pub fn connection(&self) -> watch::Receiver<bool> {
        self.inner.connected.subscribe()
    }

    /// Send a request and wait for the answer. Survives reconnects (the request is re-sent).
    /// Runtime-agnostic: can be awaited outside Tokio (e.g. on the GPUI executor).
    pub async fn request(&self, to: &str, method: &str, params: Value) -> Result<Value, String> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let line = Msg::Request {
            id,
            to: to.to_string(),
            from: String::new(),
            method: method.to_string(),
            params,
        }
        .line();
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.inner.state.lock().unwrap();
            if let Some(out) = &st.out {
                let _ = out.send(line.clone());
            }
            st.pending.insert(id, Pending { line, tx });
        }
        let res = rx.await.unwrap_or_else(|_| Err("bus client dropped".into()));
        res
    }

    /// Like [`Bus::request`] with a timeout. Needs a Tokio runtime.
    pub async fn request_timeout(
        &self,
        to: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        match tokio::time::timeout(timeout, self.request(to, method, params)).await {
            Ok(r) => r,
            Err(_) => Err(format!("timeout calling {to}.{method}")),
        }
    }

    /// Fire-and-forget event. Dropped while disconnected.
    pub fn emit(&self, topic: &str, data: Value) {
        let line = Msg::Event { topic: topic.to_string(), data }.line();
        let st = self.inner.state.lock().unwrap();
        if let Some(out) = &st.out {
            let _ = out.send(line);
        }
    }
}

async fn run(bus: Bus, hello: String, handler: Option<Handler>, ev_tx: mpsc::UnboundedSender<(String, Value)>) {
    let mut backoff = Duration::from_millis(100);
    loop {
        let name = match paths::core_socket().to_ns_name::<GenericNamespaced>() {
            Ok(n) => n,
            Err(e) => {
                tracing::error!("bad socket name: {e}");
                return;
            }
        };
        match Stream::connect(name).await {
            Ok(stream) => {
                backoff = Duration::from_millis(100);
                serve(&bus, stream, &hello, handler.as_ref(), &ev_tx).await;
                tracing::info!("bus connection lost; reconnecting");
            }
            Err(_) => {}
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(1));
    }
}

async fn serve(
    bus: &Bus,
    stream: Stream,
    hello: &str,
    handler: Option<&Handler>,
    ev_tx: &mpsc::UnboundedSender<(String, Value)>,
) {
    #[allow(deprecated)]
    let (r, mut w) = stream.split();
    if w.write_all(hello.as_bytes()).await.is_err() {
        return;
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    {
        // Re-send everything still unanswered, then publish the new sender. Done under the
        // lock so a concurrent `request` is sent exactly once.
        let mut st = bus.inner.state.lock().unwrap();
        for p in st.pending.values() {
            let _ = tx.send(p.line.clone());
        }
        st.out = Some(tx.clone());
    }
    let _ = bus.inner.connected.send_replace(true);

    let mut lines = BufReader::new(r).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => dispatch(bus, &line, handler, &tx, ev_tx),
                    _ => break,
                }
            }
            out = rx.recv() => {
                match out {
                    Some(l) => if w.write_all(l.as_bytes()).await.is_err() { break },
                    None => break,
                }
            }
        }
    }
    bus.inner.state.lock().unwrap().out = None;
    let _ = bus.inner.connected.send_replace(false);
}

fn dispatch(
    bus: &Bus,
    line: &str,
    handler: Option<&Handler>,
    tx: &mpsc::UnboundedSender<String>,
    ev_tx: &mpsc::UnboundedSender<(String, Value)>,
) {
    let msg: Msg = match serde_json::from_str(line) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("bad bus line: {e}");
            return;
        }
    };
    match msg {
        Msg::Response { id, ok, err } => {
            let p = bus.inner.state.lock().unwrap().pending.remove(&id);
            if let Some(p) = p {
                let _ = p.tx.send(match err {
                    Some(e) => Err(e),
                    None => Ok(ok.unwrap_or(Value::Null)),
                });
            }
        }
        Msg::Request { id, from, method, params, .. } => {
            let tx = tx.clone();
            match handler {
                Some(h) => {
                    let fut = h(Incoming { from, method, params });
                    tokio::spawn(async move {
                        let res = fut.await;
                        let _ = tx.send(Msg::response(id, res).line());
                    });
                }
                None => {
                    let _ = tx.send(Msg::response(id, Err(format!("{} handles no requests", bus.inner.role))).line());
                }
            }
        }
        Msg::Event { topic, data } => {
            let _ = ev_tx.send((topic, data));
        }
        Msg::Welcome { .. } | Msg::Hello { .. } => {}
    }
}

/// Exit the process shortly after the current response has been flushed.
pub fn exit_soon(code: i32) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        std::process::exit(code);
    });
}
