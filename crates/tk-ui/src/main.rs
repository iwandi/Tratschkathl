//! L4 UI: the multi-document workbench (spec §4.5) built on GPUI. Holds no domain state:
//! sessions come from Services, terminal frames from the Agent Handler. The layout is handed
//! to the next UI version on reload.

mod markdown;
mod termdraw;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    div, font, px, rgb, App, Application, Bounds, ClickEvent, Context, FocusHandle, FontWeight, KeyDownEvent,
    MouseButton, MouseDownEvent, PathPromptOptions, ScrollHandle, ScrollWheelEvent, SharedString, TitlebarOptions, Window,
    WindowBounds, WindowOptions,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use tk_bus::frame::{Frame, KeyInput};
use tk_bus::model::{
    AgentConfig, ChatItem, DocRef, Document, Fact, FactInput, RenderedView, SessionInfo, TreeNode, DOC_PREFIX,
};
use tk_bus::settings::Settings;
use tk_bus::{client::handler, Bus};

const LEFT_W: f32 = 250.0;
const RIGHT_W: f32 = 250.0;
const STATUS_H: f32 = 26.0;
const TAB_H: f32 = 32.0;
const PAD: f32 = 6.0;
const CHAIN_H: f32 = 36.0;
const VIEW_PREFIX: &str = "tk://view/";

const BG: u32 = 0x181818;
const PANEL: u32 = 0x202020;
const BORDER: u32 = 0x333333;
const TEXT: u32 = 0xcccccc;
const DIM: u32 = 0x888888;
const ACCENT: u32 = 0x3b8eea;
const BTN: u32 = 0x2d2d2d;
const BTN_HOVER: u32 = 0x3a3a3a;
const TAB_INACTIVE: u32 = 0x181818;
const TERM_BG: u32 = tk_term_bg();

const fn tk_term_bg() -> u32 {
    0x1e1e1e
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct PaneState {
    tabs: Vec<String>,
    active: usize,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct LayoutState {
    panes: Vec<PaneState>,
    active_pane: usize,
    /// Unsent chat drafts by session.
    #[serde(default)]
    drafts: HashMap<String, String>,
}

struct Pane {
    tabs: Vec<String>,
    active: usize,
    focus: FocusHandle,
}

enum UiCmd {
    Shutdown(oneshot::Sender<()>),
}

struct Workbench {
    bus: Bus,
    sessions: Vec<SessionInfo>,
    agents: Vec<AgentConfig>,
    panes: Vec<Pane>,
    active_pane: usize,
    frames: HashMap<String, Frame>,
    sent_sizes: HashMap<String, (u16, u16)>,
    notices: Vec<String>,
    connected: bool,
    font_family: SharedString,
    font_size: f32,
    cell_w: f32,
    line_h: f32,
    want_focus: bool,
    scroll_acc: f32,
    /// Rendered views from Services (spec §4.3).
    views: Vec<RenderedView>,
    /// Collapsed tree nodes, by node key.
    collapsed: HashSet<String>,
    /// Node whose "+" agent picker is open.
    create_at: Option<String>,
    /// Facts of the context session (session id, facts).
    active_facts: (Option<String>, Vec<Fact>),
    /// Documents offered for the context session.
    active_docs: Vec<DocRef>,
    /// The session the context dock shows: the active tab, or the last session tab when a
    /// document tab is active.
    last_session: Option<String>,
    /// Loaded documents, refreshed while visible.
    docs: HashMap<String, Document>,
    doc_errors: HashMap<String, String>,
    doc_inflight: HashSet<String>,
    /// Collapsed files in changeset documents, by `uri#path`.
    collapsed_files: HashSet<String>,
    /// Chat transcripts by session, with the agent state.
    chats: HashMap<String, (Vec<ChatItem>, String)>,
    chat_scroll: HashMap<String, ScrollHandle>,
    /// Unsent chat input by session.
    drafts: HashMap<String, String>,
    chat_requested: HashSet<String>,
}

fn is_doc(id: &str) -> bool {
    id.starts_with(DOC_PREFIX)
}

fn is_view(id: &str) -> bool {
    id.starts_with(VIEW_PREFIX)
}

/// A board card being dragged.
#[derive(Clone)]
struct CardDrag {
    session: String,
    title: String,
}

impl Render for CardDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(rgb(0x264f78))
            .text_color(rgb(0xffffff))
            .text_sm()
            .shadow_md()
            .child(self.title.clone())
    }
}

fn main() {
    if std::env::args().any(|a| a == "--self-test") {
        println!("{}", tk_bus::self_test_json("ui"));
        return;
    }
    tk_bus::logging::init("ui");

    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio");
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCmd>();
    let h = handler(move |inc| {
        let cmd_tx = cmd_tx.clone();
        async move {
            match inc.method.as_str() {
                "layer.shutdown" => {
                    let (tx, rx) = oneshot::channel();
                    let _ = cmd_tx.send(UiCmd::Shutdown(tx));
                    let _ = tokio::time::timeout(Duration::from_secs(3), rx).await;
                    tk_bus::client::exit_soon(0);
                    Ok(json!(true))
                }
                other => Err(format!("ui: unknown method {other}")),
            }
        }
    });
    let (bus, events) = {
        let _guard = rt.enter();
        Bus::connect(
            "ui",
            env!("CARGO_PKG_VERSION"),
            vec![
                "term.".into(),
                "sessions.".into(),
                "host.".into(),
                "layer.".into(),
                "workbench.".into(),
                "facts.".into(),
                "views.".into(),
                "chat.".into(),
                "decisions.".into(),
            ],
            Some(h),
        )
    };
    let settings = Settings::load();
    let rt_handle = rt.handle().clone();

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, gpui::size(px(1500.0), px(950.0)), cx);
        let bus_for_close = bus.clone();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("Tratschkathl".into()), ..Default::default() }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Workbench::new(bus, events, cmd_rx, settings, window, cx)),
        )
        .expect("open window");
        cx.on_window_closed(move |cx| {
            // The user closed the window: quit the whole app (agents included).
            let bus = bus_for_close.clone();
            let _ = rt_handle.block_on(async move {
                tokio::time::timeout(Duration::from_secs(2), bus.request("core", "app.quit", json!({}))).await
            });
            cx.quit();
        })
        .detach();
        cx.activate(true);
    });
    drop(rt);
}

impl Workbench {
    fn new(
        bus: Bus,
        mut events: tk_bus::client::Events,
        mut cmds: mpsc::UnboundedReceiver<UiCmd>,
        settings: Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let font_family: SharedString = settings.font_family.clone().into();
        let font_size = settings.font_size;
        let font_id = window.text_system().resolve_font(&font(font_family.clone()));
        let cell_w = window
            .text_system()
            .advance(font_id, px(font_size), 'm')
            .map(|s| f32::from(s.width))
            .unwrap_or(font_size * 0.6);
        let line_h = (font_size * 1.3).round();

        // Bus events → workbench.
        cx.spawn(async move |this, cx| {
            while let Some((topic, data)) = events.recv().await {
                if this.update(cx, |wb, cx| wb.on_event(&topic, data, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        // Commands from the bus handler (shutdown for reload).
        cx.spawn(async move |this, cx| {
            while let Some(cmd) = cmds.recv().await {
                match cmd {
                    UiCmd::Shutdown(done) => {
                        let Ok((bus, layout)) = this.update(cx, |wb, _| (wb.bus.clone(), wb.layout_state())) else {
                            break;
                        };
                        let blob = serde_json::to_value(layout).unwrap_or(Value::Null);
                        let _ = bus.request("core", "handoff.put", json!({ "unit": "ui", "blob": blob })).await;
                        let _ = done.send(());
                    }
                }
            }
        })
        .detach();

        // Connection state.
        let mut conn = bus.connection();
        cx.spawn(async move |this, cx| {
            loop {
                let up = *conn.borrow_and_update();
                if this
                    .update(cx, |wb, cx| {
                        wb.connected = up;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                if conn.changed().await.is_err() {
                    break;
                }
            }
        })
        .detach();

        // Initial load: handoff from the previous UI version, sessions, agents.
        let bus2 = bus.clone();
        cx.spawn(async move |this, cx| {
            let layout = bus2.request("core", "handoff.take", json!({ "unit": "ui" })).await.ok();
            let layout: Option<LayoutState> = layout.and_then(|v| serde_json::from_value(v).ok());
            let agents = bus2.request("agents", "agents.list", json!({})).await.unwrap_or(json!([]));
            let sessions = bus2.request("services", "session.list", json!({})).await.unwrap_or(json!([]));
            let _ = this.update(cx, |wb, cx| {
                wb.agents = serde_json::from_value(agents).unwrap_or_default();
                wb.sessions = serde_json::from_value(sessions).unwrap_or_default();
                if let Some(layout) = layout {
                    wb.restore_layout(layout, cx);
                    wb.notice("UI reloaded; layout restored");
                }
                wb.refresh_views(cx);
                cx.notify();
            });
            let _ = bus2.request("agents", "term.refresh", json!({})).await;
        })
        .detach();

        // Visible documents are live: reload them every few seconds.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            let Ok(visible) = this.update(cx, |wb, _| wb.visible_docs()) else { break };
            for uri in visible {
                if this.update(cx, |wb, cx| wb.fetch_doc(uri.clone(), cx)).is_err() {
                    return;
                }
            }
        })
        .detach();

        let focus = cx.focus_handle();
        Workbench {
            bus,
            sessions: vec![],
            agents: vec![],
            panes: vec![Pane { tabs: vec![], active: 0, focus }],
            active_pane: 0,
            frames: HashMap::new(),
            sent_sizes: HashMap::new(),
            notices: vec![],
            connected: false,
            font_family,
            font_size,
            cell_w,
            line_h,
            want_focus: true,
            scroll_acc: 0.0,
            views: vec![],
            collapsed: HashSet::new(),
            create_at: None,
            active_facts: (None, vec![]),
            active_docs: vec![],
            last_session: None,
            docs: HashMap::new(),
            doc_errors: HashMap::new(),
            doc_inflight: HashSet::new(),
            collapsed_files: HashSet::new(),
            chats: HashMap::new(),
            chat_scroll: HashMap::new(),
            drafts: HashMap::new(),
            chat_requested: HashSet::new(),
        }
    }

    // ---- state -------------------------------------------------------------------------

    fn notice(&mut self, text: impl Into<String>) {
        self.notices.push(text.into());
        if self.notices.len() > 50 {
            self.notices.remove(0);
        }
    }

    fn layout_state(&self) -> LayoutState {
        LayoutState {
            panes: self.panes.iter().map(|p| PaneState { tabs: p.tabs.clone(), active: p.active }).collect(),
            active_pane: self.active_pane,
            drafts: self.drafts.clone(),
        }
    }

    fn restore_layout(&mut self, layout: LayoutState, cx: &mut Context<Self>) {
        if layout.panes.is_empty() {
            return;
        }
        self.panes = layout
            .panes
            .into_iter()
            .map(|p| Pane { active: p.active.min(p.tabs.len().saturating_sub(1)), tabs: p.tabs, focus: cx.focus_handle() })
            .collect();
        self.active_pane = layout.active_pane.min(self.panes.len() - 1);
        self.drafts = layout.drafts;
        self.want_focus = true;
    }

    fn session(&self, id: &str) -> Option<&SessionInfo> {
        self.sessions.iter().find(|s| s.id == id)
    }

    /// Title shown for a session or document; sessions with the same base title get a short
    /// id suffix so two "Claude Code" tabs can be told apart.
    fn session_title(&self, id: &str) -> String {
        let base = self.base_title(id);
        if is_doc(id) || is_view(id) {
            return base;
        }
        let twins = self.sessions.iter().filter(|s| s.id != id && self.base_title(&s.id) == base).count();
        if twins > 0 {
            format!("{base} · {}", &id[id.len().saturating_sub(4)..])
        } else {
            base
        }
    }

    /// A one-character marker for what a tab holds.
    fn kind_marker(&self, id: &str) -> &'static str {
        if is_view(id) {
            return "▦";
        }
        if is_doc(id) {
            return if id.contains("/changeset/") {
                "±"
            } else if id.contains("/decision-list/") {
                "⚖"
            } else {
                "¶"
            };
        }
        match self.session(id).map(|s| s.kind.as_str()) {
            Some("chat") => "◆",
            _ => "❯",
        }
    }

    fn kind_color(&self, id: &str) -> u32 {
        if is_view(id) {
            return 0x3b8eea;
        }
        if is_doc(id) {
            return 0xe5c07b;
        }
        match self.session(id).map(|s| s.kind.as_str()) {
            Some("chat") => 0xd670d6,
            _ => 0x23d18b,
        }
    }

    fn base_title(&self, id: &str) -> String {
        if let Some(view_id) = id.strip_prefix(VIEW_PREFIX) {
            return self
                .views
                .iter()
                .find(|v| v.view.id == view_id)
                .and_then(|v| v.view.title.clone())
                .unwrap_or_else(|| view_id.to_string());
        }
        if is_doc(id) {
            return self
                .docs
                .get(id)
                .map(|d| d.title.clone())
                .or_else(|| self.active_docs.iter().find(|d| d.uri == id).map(|d| d.title.clone()))
                .unwrap_or_else(|| id.rsplit('/').next().unwrap_or(id).to_string());
        }
        match self.session(id) {
            Some(s) => {
                let folder = std::path::Path::new(&s.cwd)
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let mut title = s.title.trim_start_matches(|c: char| !c.is_alphanumeric()).to_string();
                // Shells set the window title to their executable path: show just its name.
                if title.contains('\\') || title.contains('/') {
                    title = std::path::Path::new(&title)
                        .file_stem()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or(title);
                }
                if folder.is_empty() { title } else { format!("{title} · {folder}") }
            }
            None => id.to_string(),
        }
    }

    fn open_tab(&mut self, id: &str) {
        for (pi, p) in self.panes.iter_mut().enumerate() {
            if let Some(ti) = p.tabs.iter().position(|t| t == id) {
                p.active = ti;
                self.active_pane = pi;
                self.want_focus = true;
                return;
            }
        }
        let pane = &mut self.panes[self.active_pane];
        pane.tabs.push(id.to_string());
        pane.active = pane.tabs.len() - 1;
        self.want_focus = true;
    }

    fn close_tab(&mut self, pi: usize, ti: usize) {
        let Some(p) = self.panes.get_mut(pi) else { return };
        if ti < p.tabs.len() {
            p.tabs.remove(ti);
            p.active = p.active.min(p.tabs.len().saturating_sub(1));
        }
        if p.tabs.is_empty() && self.panes.len() > 1 {
            self.panes.remove(pi);
            self.active_pane = self.active_pane.min(self.panes.len() - 1);
        }
        self.sent_sizes.clear();
    }

    fn split(&mut self, pi: usize, cx: &mut Context<Self>) {
        let p = &mut self.panes[pi];
        let mut tabs = vec![];
        if p.tabs.len() > 1 {
            let t = p.tabs.remove(p.active);
            p.active = p.active.min(p.tabs.len() - 1);
            tabs.push(t);
        }
        self.panes.insert(pi + 1, Pane { tabs, active: 0, focus: cx.focus_handle() });
        self.active_pane = pi + 1;
        self.sent_sizes.clear();
        self.want_focus = true;
    }

    fn refresh_sessions(&self, cx: &mut Context<Self>) {
        let bus = self.bus.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(v) = bus.request("services", "session.list", json!({})).await {
                let _ = this.update(cx, |wb, cx| {
                    wb.sessions = serde_json::from_value(v).unwrap_or_default();
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn on_event(&mut self, topic: &str, data: Value, cx: &mut Context<Self>) {
        match topic {
            "term.frame" => {
                if let (Some(id), Ok(frame)) =
                    (data["session"].as_str(), serde_json::from_value::<Frame>(data["frame"].clone()))
                {
                    self.frames.insert(id.to_string(), frame);
                }
            }
            "term.ended" | "sessions.changed" => {
                self.refresh_sessions(cx);
                self.refresh_views(cx);
            }
            "facts.changed" => {
                self.refresh_views(cx);
                if data["session"].as_str() == self.active_facts.0.as_deref() {
                    self.active_facts.0 = None; // facts and documents refetched on next render
                }
            }
            "views.changed" => self.refresh_views(cx),
            "chat.items" => self.apply_chat(&data),
            "decisions.changed" => {
                for uri in self.visible_docs().into_iter().filter(|u| u.contains("/decision-list/")) {
                    self.fetch_doc(uri, cx);
                }
            }
            "workbench.open_document" => {
                if let Some(uri) = data["uri"].as_str() {
                    if data["placement"].as_str() == Some("split") {
                        let last = self.panes.len() - 1;
                        self.split(last, cx);
                    }
                    self.open_tab(uri);
                    self.fetch_doc(uri.to_string(), cx);
                }
            }
            "workbench.focus" => {
                if let Some(id) = data["session"].as_str() {
                    self.open_tab(id);
                    self.refresh_sessions(cx);
                }
            }
            "host.notice" => self.notice(data["text"].as_str().unwrap_or("").to_string()),
            "host.reload" => {
                let ok = data["ok"].as_bool().unwrap_or(false);
                self.notice(if ok { "Reload finished".to_string() } else { format!("Reload failed: {}", data["failed"]) });
            }
            "layer.up" => match data["role"].as_str() {
                Some("agents") => {
                    self.sent_sizes.clear();
                    let bus = self.bus.clone();
                    cx.spawn(async move |_, _| {
                        let _ = bus.request("agents", "term.refresh", json!({})).await;
                    })
                    .detach();
                    let agents_bus = self.bus.clone();
                    cx.spawn(async move |this, cx| {
                        if let Ok(v) = agents_bus.request("agents", "agents.list", json!({})).await {
                            let _ = this.update(cx, |wb, _| wb.agents = serde_json::from_value(v).unwrap_or_default());
                        }
                    })
                    .detach();
                }
                Some("services") => {
                    self.refresh_sessions(cx);
                    self.refresh_views(cx);
                    self.active_facts.0 = None;
                }
                _ => {}
            },
            _ => {}
        }
        cx.notify();
    }

    /// Start a session. Without a folder the user picks one; `facts` pre-seed the session
    /// ("+" on a view node, spec §4.3).
    fn new_session(&mut self, agent: String, cwd: Option<String>, facts: Vec<FactInput>, cx: &mut Context<Self>) {
        let rx = match cwd {
            Some(_) => None,
            None => Some(cx.prompt_for_paths(PathPromptOptions {
                files: false,
                directories: true,
                multiple: false,
                prompt: Some("Choose the working folder".into()),
            })),
        };
        let bus = self.bus.clone();
        cx.spawn(async move |this, cx| {
            let dir = match (cwd, rx) {
                (Some(c), _) => c,
                (None, Some(rx)) => {
                    let Ok(Ok(Some(paths))) = rx.await else { return };
                    let Some(dir) = paths.into_iter().next() else { return };
                    dir.to_string_lossy().into_owned()
                }
                (None, None) => return,
            };
            let res = bus
                .request("agents", "session.create", json!({ "agent": agent, "cwd": dir, "facts": facts }))
                .await;
            let _ = this.update(cx, |wb, cx| {
                match res.and_then(|v| serde_json::from_value::<SessionInfo>(v).map_err(|e| e.to_string())) {
                    Ok(info) => {
                        let id = info.id.clone();
                        if !wb.sessions.iter().any(|s| s.id == id) {
                            wb.sessions.insert(0, info);
                        }
                        wb.open_tab(&id);
                    }
                    Err(e) => wb.notice(format!("Could not start session: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_views(&self, cx: &mut Context<Self>) {
        let bus = self.bus.clone();
        // `$selected.<key>` in view filters resolves against the context session's facts.
        let mut selection = serde_json::Map::new();
        for f in &self.active_facts.1 {
            selection.entry(f.key.clone()).or_insert(json!(f.value));
        }
        cx.spawn(async move |this, cx| {
            let res = bus.request("services", "view.render", json!({ "selection": selection })).await;
            let _ = this.update(cx, |wb, cx| {
                match res.and_then(|v| serde_json::from_value::<Vec<RenderedView>>(v).map_err(|e| e.to_string())) {
                    Ok(v) => wb.views = v,
                    Err(e) => wb.notice(format!("Views: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Keep the context dock's facts in step with the active session.
    fn active_tab(&self) -> Option<String> {
        self.panes.get(self.active_pane).and_then(|p| p.tabs.get(p.active)).cloned()
    }

    /// The session the context dock is about.
    fn context_session(&self) -> Option<String> {
        match self.active_tab() {
            Some(t) if !is_doc(&t) && !is_view(&t) => Some(t),
            _ => self.last_session.clone(),
        }
    }

    /// Keep the context dock's facts and documents in step with the context session.
    fn sync_active_facts(&mut self, cx: &mut Context<Self>) {
        if let Some(t) = self.active_tab().filter(|t| !is_doc(t) && !is_view(t)) {
            self.last_session = Some(t);
        }
        let ctx = self.context_session();
        if ctx == self.active_facts.0 && ctx.is_some() {
            return;
        }
        let Some(sid) = ctx else {
            self.active_facts = (None, vec![]);
            self.active_docs.clear();
            return;
        };
        self.active_facts.0 = Some(sid.clone());
        let bus = self.bus.clone();
        cx.spawn(async move |this, cx| {
            let facts = bus.request("services", "fact.list", json!({ "session": sid })).await;
            let docs = bus.request("services", "document.list", json!({ "session": sid })).await;
            let _ = this.update(cx, |wb, cx| {
                if wb.active_facts.0.as_deref() == Some(sid.as_str()) {
                    wb.active_facts.1 = facts.ok().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
                    wb.active_docs = docs.ok().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
                    wb.refresh_views(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Document tabs that are currently shown in some pane.
    fn visible_docs(&self) -> Vec<String> {
        self.panes.iter().filter_map(|p| p.tabs.get(p.active)).filter(|t| is_doc(t)).cloned().collect()
    }

    fn fetch_doc(&mut self, uri: String, cx: &mut Context<Self>) {
        if !self.doc_inflight.insert(uri.clone()) {
            return;
        }
        let bus = self.bus.clone();
        cx.spawn(async move |this, cx| {
            let res = bus.request("services", "document.get", json!({ "uri": uri })).await;
            let _ = this.update(cx, |wb, cx| {
                wb.doc_inflight.remove(&uri);
                match res.and_then(|v| serde_json::from_value::<Document>(v).map_err(|e| e.to_string())) {
                    Ok(doc) => {
                        wb.doc_errors.remove(&uri);
                        wb.docs.insert(uri, doc);
                    }
                    Err(e) => {
                        wb.doc_errors.insert(uri, e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn on_key(&mut self, sid: &str, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let m = &ks.modifiers;
        let paste = (m.control && !m.alt && ks.key == "v") || (m.shift && ks.key == "insert");
        if paste {
            if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                self.bus.emit("ui.paste", json!({ "session": sid, "text": text }));
            }
            return;
        }
        let k = KeyInput { key: ks.key.clone(), key_char: ks.key_char.clone(), ctrl: m.control, alt: m.alt, shift: m.shift };
        self.bus.emit("ui.key", json!({ "session": sid, "key": k }));
    }

    fn on_scroll(&mut self, sid: &str, ev: &ScrollWheelEvent) {
        let dy = f32::from(ev.delta.pixel_delta(px(self.line_h)).y);
        self.scroll_acc += dy / self.line_h;
        let lines = self.scroll_acc.trunc();
        if lines != 0.0 {
            self.scroll_acc -= lines;
            self.bus.emit("ui.scroll", json!({ "session": sid, "delta": lines as i32 }));
        }
    }

    /// Tell the Agent Handler how big each visible terminal is.
    fn sync_sizes(&mut self, window: &Window) {
        let vp = window.viewport_size();
        let w = f32::from(vp.width);
        let h = f32::from(vp.height);
        let n = self.panes.len().max(1) as f32;
        let pane_w = (w - LEFT_W - RIGHT_W) / n - 2.0 * PAD - 2.0;
        let chain_h = if self.views.iter().any(|v| v.view.dock == "top") { CHAIN_H } else { 0.0 };
        let pane_h = h - STATUS_H - TAB_H - chain_h - 2.0 * PAD - 2.0;
        let cols = (pane_w / self.cell_w).floor().max(10.0) as u16;
        let rows = (pane_h / self.line_h).floor().max(4.0) as u16;
        for p in &self.panes {
            if let Some(sid) = p.tabs.get(p.active).filter(|t| !is_doc(t) && !is_view(t) && !self.is_chat(t)) {
                if self.sent_sizes.get(sid) != Some(&(cols, rows)) {
                    self.sent_sizes.insert(sid.clone(), (cols, rows));
                    self.bus.emit("ui.resize", json!({ "session": sid, "cols": cols, "rows": rows }));
                }
            }
        }
    }

    // ---- rendering ---------------------------------------------------------------------

    fn render_left(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_sid = self.panes.get(self.active_pane).and_then(|p| p.tabs.get(p.active)).cloned();
        div()
            .w(px(LEFT_W))
            .h_full()
            .flex()
            .flex_col()
            .flex_none()
            .bg(rgb(PANEL))
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(section_header("NEW SESSION"))
            .children(self.agents.iter().enumerate().map(|(i, a)| {
                let id = a.id.clone();
                div()
                    .id(("new-agent", i))
                    .mx_2()
                    .my_1()
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(BTN))
                    .hover(|s| s.bg(rgb(BTN_HOVER)))
                    .cursor_pointer()
                    .child(format!("+ {}", a.name))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.new_session(id.clone(), None, vec![], cx)))
            }))
            .child({
                // Views and the plain session list share one scroll area.
                let mut rows: Vec<gpui::AnyElement> = vec![];
                let open_views: Vec<(String, String)> = self
                    .views
                    .iter()
                    .filter(|v| v.view.dock == "center")
                    .map(|v| (v.view.id.clone(), v.view.title.clone().unwrap_or_else(|| v.view.id.clone())))
                    .collect();
                if !open_views.is_empty() {
                    rows.push(section_header("VIEWS").into_any_element());
                    for (i, (id, title)) in open_views.into_iter().enumerate() {
                        let uri = format!("{VIEW_PREFIX}{id}");
                        let selected = active_sid.as_deref() == Some(uri.as_str());
                        rows.push(
                            div()
                                .id(("view-entry", i))
                                .px_3()
                                .py_1()
                                .flex()
                                .flex_row()
                                .gap_2()
                                .cursor_pointer()
                                .when(selected, |d| d.bg(rgb(BTN_HOVER)))
                                .hover(|d| d.bg(rgb(BTN)))
                                .child(div().text_color(rgb(0x3b8eea)).child("▦"))
                                .child(title)
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.open_tab(&uri);
                                    cx.notify();
                                }))
                                .into_any_element(),
                        );
                    }
                }
                for v in self.views.iter().filter(|v| v.view.kind == "tree" && v.view.dock == "left") {
                    let title = v.view.title.clone().unwrap_or_else(|| v.view.id.clone()).to_uppercase();
                    rows.push(section_header(&title).into_any_element());
                    if v.root.children.is_empty() {
                        rows.push(div().px_3().text_xs().text_color(rgb(DIM)).child("no sessions").into_any_element());
                    }
                    self.tree_rows(v, &v.root, 0, active_sid.as_deref(), &mut rows, cx);
                }
                rows.push(section_header("ALL SESSIONS").into_any_element());
                for s in &self.sessions {
                    let selected = active_sid.as_deref() == Some(s.id.as_str());
                    rows.push(self.session_row(format!("all-{}", s.id), s, 0, selected, cx));
                }
                div().id("left-scroll").flex_1().overflow_y_scroll().children(rows)
            })
    }

    fn session_row(&self, key: String, s: &SessionInfo, depth: usize, selected: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let id = s.id.clone();
        let running = s.state == "running";
        div()
            .id(SharedString::from(key))
            .pl(px(12.0 + 14.0 * depth as f32))
            .pr_3()
            .py_1()
            .flex()
            .flex_col()
            .cursor_pointer()
            .when(selected, |d| d.bg(rgb(BTN_HOVER)))
            .hover(|d| d.bg(rgb(BTN)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .child(div().text_color(rgb(if running { 0x23d18b } else { 0x666666 })).child("●"))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(rgb(if running { TEXT } else { DIM }))
                            .child(self.session_title(&s.id)),
                    ),
            )
            .child(div().w_full().truncate().text_xs().text_color(rgb(DIM)).child(s.cwd.clone()))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.open_tab(&id);
                cx.notify();
            }))
            .into_any_element()
    }

    /// Flatten a tree view into rows: nodes (collapsible, with "+") and their sessions.
    fn tree_rows(
        &self,
        view: &RenderedView,
        node: &TreeNode,
        depth: usize,
        active_sid: Option<&str>,
        rows: &mut Vec<gpui::AnyElement>,
        cx: &mut Context<Self>,
    ) {
        for child in &node.children {
            let path: Vec<String> = child.facts.iter().map(|f| format!("{}={}", f.key, f.value)).collect();
            let nkey = format!("{}|{}", view.view.id, path.join("/"));
            let collapsed = self.collapsed.contains(&nkey);
            let can_create = view.view.create && !child.key.is_empty();
            let toggle_key = nkey.clone();
            let plus_key = nkey.clone();
            rows.push(
                div()
                    .id(SharedString::from(format!("node-{nkey}")))
                    .pl(px(8.0 + 14.0 * depth as f32))
                    .pr_2()
                    .py(px(3.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(BTN)))
                    .child(div().w(px(12.0)).text_xs().text_color(rgb(DIM)).child(if collapsed { "▸" } else { "▾" }))
                    .child(div().flex_none().text_xs().text_color(rgb(DIM)).child(child.key.clone()))
                    .child(div().flex_1().min_w_0().truncate().child(child.value.clone()))
                    .child(div().flex_none().text_xs().text_color(rgb(DIM)).child(child.count.to_string()))
                    .when(can_create, |d| {
                        d.child(
                            div()
                                .id(SharedString::from(format!("plus-{nkey}")))
                                .flex_none()
                                .px_1()
                                .rounded_sm()
                                .text_color(rgb(DIM))
                                .hover(|s| s.bg(rgb(BTN_HOVER)).text_color(rgb(TEXT)))
                                .child("+")
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.create_at = if this.create_at.as_deref() == Some(plus_key.as_str()) {
                                        None
                                    } else {
                                        Some(plus_key.clone())
                                    };
                                    cx.notify();
                                })),
                        )
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if !this.collapsed.remove(&toggle_key) {
                            this.collapsed.insert(toggle_key.clone());
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            );
            if self.create_at.as_deref() == Some(nkey.as_str()) {
                let mut picker = div()
                    .pl(px(22.0 + 14.0 * depth as f32))
                    .pr_2()
                    .py_1()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_1()
                    .child(div().text_xs().text_color(rgb(DIM)).child("new:"));
                for (i, a) in self.agents.iter().enumerate() {
                    let agent = a.id.clone();
                    let cwd = child.cwd.clone();
                    let facts = child.facts.clone();
                    picker = picker.child(
                        div()
                            .id(SharedString::from(format!("pick-{nkey}-{i}")))
                            .px_2()
                            .rounded_sm()
                            .text_xs()
                            .bg(rgb(BTN))
                            .hover(|s| s.bg(rgb(BTN_HOVER)))
                            .cursor_pointer()
                            .child(a.name.clone())
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.create_at = None;
                                this.new_session(agent.clone(), cwd.clone(), facts.clone(), cx);
                                cx.notify();
                            })),
                    );
                }
                rows.push(picker.into_any_element());
            }
            if !collapsed {
                self.tree_rows(view, child, depth + 1, active_sid, rows, cx);
                for sid in &child.sessions {
                    if let Some(s) = self.session(sid) {
                        let selected = active_sid == Some(sid.as_str());
                        rows.push(self.session_row(format!("tree-{nkey}-{sid}"), s, depth + 1, selected, cx));
                    }
                }
            }
        }
    }

    fn render_right(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ctx = self.context_session();
        let active = ctx.as_deref().and_then(|sid| self.session(sid));
        let row = |k: &str, v: String| {
            div()
                .px_3()
                .py_1()
                .flex()
                .flex_col()
                .child(div().text_xs().text_color(rgb(DIM)).child(k.to_string()))
                .child(div().child(v))
        };
        let mut body = div().flex().flex_col();
        match active {
            Some(s) => {
                body = body
                    .child(row("Agent", s.agent.clone()))
                    .child(row("Folder", s.cwd.clone()))
                    .child(row("State", s.state.clone()))
                    .child(row("Session", s.id.clone()))
                    .child(section_header("FACTS"))
                    .child(self.render_facts(&s.id))
                    .child(section_header("DOCUMENTS"))
                    .child(self.render_doc_list(cx));
            }
            None => body = body.child(div().px_3().text_color(rgb(DIM)).child("No active session")),
        }
        div()
            .w(px(RIGHT_W))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(rgb(PANEL))
            .border_l_1()
            .border_color(rgb(BORDER))
            .child(section_header("CONTEXT"))
            .child(body)
    }

    fn render_facts(&self, sid: &str) -> gpui::AnyElement {
        let facts: &[Fact] = if self.active_facts.0.as_deref() == Some(sid) { &self.active_facts.1 } else { &[] };
        if facts.is_empty() {
            return div().px_3().text_color(rgb(DIM)).child("none").into_any_element();
        }
        div()
            .flex()
            .flex_col()
            .children(facts.iter().map(|f| {
                div()
                    .px_3()
                    .py(px(2.0))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(div().flex_none().text_color(rgb(DIM)).child(f.key.clone()))
                            .child(div().flex_1().min_w_0().truncate().child(f.value.clone())),
                    )
                    .child(div().text_xs().text_color(rgb(0x666666)).child(match &f.kind {
                        Some(k) => format!("{} · {k}", f.source),
                        None => f.source.clone(),
                    }))
            }))
            .into_any_element()
    }

    fn is_chat(&self, id: &str) -> bool {
        self.session(id).is_some_and(|s| s.kind == "chat")
    }

    fn apply_chat(&mut self, data: &Value) {
        let Some(sid) = data["session"].as_str() else { return };
        let from = data["from"].as_u64().unwrap_or(0) as usize;
        let items: Vec<ChatItem> = serde_json::from_value(data["items"].clone()).unwrap_or_default();
        let state = data["state"].as_str().unwrap_or("").to_string();
        let entry = self.chats.entry(sid.to_string()).or_insert_with(|| (vec![], String::new()));
        let grew = from + items.len() >= entry.0.len();
        entry.0.truncate(from);
        entry.0.extend(items);
        entry.1 = state;
        if grew {
            if let Some(h) = self.chat_scroll.get(sid) {
                h.scroll_to_bottom();
            }
        }
    }

    /// Load the transcript of a chat tab that has none yet (UI start, ended sessions).
    fn ensure_chat(&mut self, sid: &str, cx: &mut Context<Self>) {
        if self.chats.contains_key(sid) || !self.chat_requested.insert(sid.to_string()) {
            return;
        }
        let bus = self.bus.clone();
        let sid = sid.to_string();
        cx.spawn(async move |this, cx| {
            if let Ok(v) = bus.request("agents", "chat.history", json!({ "session": sid })).await {
                let _ = this.update(cx, |wb, cx| {
                    if !wb.chats.contains_key(&sid) {
                        wb.apply_chat(&v);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    fn chat_call(&self, method: &str, params: Value, cx: &mut Context<Self>) {
        let bus = self.bus.clone();
        let method = method.to_string();
        cx.spawn(async move |this, cx| {
            if let Err(e) = bus.request("agents", &method, params).await {
                let _ = this.update(cx, |wb, cx| {
                    wb.notice(format!("Chat: {e}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn on_chat_key(&mut self, sid: &str, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let m = &ks.modifiers;
        let draft = self.drafts.entry(sid.to_string()).or_default();
        match ks.key.as_str() {
            "enter" if !m.shift => {
                let text = draft.trim().to_string();
                if !text.is_empty() {
                    draft.clear();
                    self.chat_call("chat.send", json!({ "session": sid, "text": text }), cx);
                }
            }
            "enter" => draft.push('\n'),
            "backspace" => {
                if m.control {
                    let keep = draft.trim_end().rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
                    draft.truncate(keep);
                } else {
                    draft.pop();
                }
            }
            "escape" => self.chat_call("chat.cancel", json!({ "session": sid }), cx),
            "space" => draft.push(' '),
            "tab" => draft.push_str("    "),
            "v" if m.control && !m.alt => {
                if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    draft.push_str(&text.replace("\r\n", "\n"));
                }
            }
            _ => {
                if let Some(c) = &ks.key_char {
                    if !(m.control && !m.alt) {
                        draft.push_str(c);
                    }
                }
            }
        }
        cx.notify();
    }

    fn render_chat(&self, pi: usize, sid: &str, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (items, state) = self.chats.get(sid).map(|(i, s)| (i.as_slice(), s.as_str())).unwrap_or((&[], "loading"));
        let scroll = self.chat_scroll.get(sid).cloned().unwrap_or_default();
        let focus = self.panes[pi].focus.clone();
        let focus2 = focus.clone();
        let sid_key = sid.to_string();
        let mono = self.font_family.clone();
        let mut list = div()
            .id(SharedString::from(format!("chat-{pi}")))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .p_4()
            .flex()
            .flex_col()
            .gap_3();
        for (ix, item) in items.iter().enumerate() {
            list = list.child(self.render_chat_item(sid, ix, item, mono.clone(), cx));
        }
        let draft = self.drafts.get(sid).cloned().unwrap_or_default();
        let focused = self.active_pane == pi;
        let (state_text, state_color) = match state {
            "starting" => ("Starting the agent…", 0xe5e510),
            "working" => ("Agent is working… (Esc to cancel)", 0x3b8eea),
            "waiting" => ("Waiting for your permission", 0xe5c07b),
            "idle" => ("Ready", 0x23d18b),
            "error" => ("Error", 0xf14c4c),
            "ended" => ("Agent process ended", 0x888888),
            other => (other, 0x888888),
        };
        let input = div()
            .id(SharedString::from(format!("chat-input-{pi}")))
            .track_focus(&focus)
            .mx_4()
            .mb_3()
            .p_2()
            .min_h(px(44.0))
            .rounded_md()
            .border_1()
            .border_color(rgb(if focused { ACCENT } else { BORDER }))
            .bg(rgb(PANEL))
            .cursor_text()
            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
                cx.stop_propagation();
                this.on_chat_key(&sid_key, ev, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.active_pane = pi;
                    window.focus(&focus2);
                    cx.notify();
                }),
            )
            .child(if draft.is_empty() {
                div()
                    .text_color(rgb(DIM))
                    .child("Message the agent. Enter sends, Shift+Enter adds a line, Esc cancels the turn.")
                    .into_any_element()
            } else {
                div()
                    .flex()
                    .flex_col()
                    .children(draft.split('\n').map(|l| div().min_h(px(18.0)).child(l.to_string())).collect::<Vec<_>>())
                    .into_any_element()
            })
            .when(focused, |d| d.child(div().w(px(8.0)).h(px(2.0)).bg(rgb(TEXT))));
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(rgb(TERM_BG))
            .child(list)
            .child(div().px_4().pb_1().text_xs().text_color(rgb(state_color)).child(state_text.to_string()))
            .child(input)
            .into_any_element()
    }

    fn render_chat_item(&self, sid: &str, ix: usize, item: &ChatItem, mono: SharedString, cx: &mut Context<Self>) -> gpui::AnyElement {
        let card = |border: u32| div().p_3().rounded_md().border_1().border_color(rgb(border)).bg(rgb(PANEL)).flex().flex_col().gap_1();
        let limited = |text: &str, n: usize| -> String {
            let lines: Vec<&str> = text.lines().collect();
            if lines.len() > n {
                format!("{}\n… {} more lines", lines[..n].join("\n"), lines.len() - n)
            } else {
                text.to_string()
            }
        };
        match item.kind.as_str() {
            "user" => div()
                .flex()
                .flex_row()
                .child(div().flex_1())
                .child(div().max_w(px(640.0)).p_3().rounded_md().bg(rgb(0x264f78)).child(item.text.clone()))
                .into_any_element(),
            "agent" => markdown::render(&item.text, mono),
            "thought" => div()
                .pl_3()
                .border_l_2()
                .border_color(rgb(0x444444))
                .text_color(rgb(DIM))
                .italic()
                .child(limited(&item.text, 4))
                .into_any_element(),
            "tool" => {
                let status = item.status.clone().unwrap_or_default();
                let (icon, color) = match status.as_str() {
                    "completed" => ("●", 0x23d18b),
                    "failed" => ("✕", 0xf14c4c),
                    "in_progress" => ("◐", 0x3b8eea),
                    _ => ("○", 0x888888),
                };
                card(BORDER)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(div().text_color(rgb(color)).child(icon))
                            .child(div().flex_1().min_w_0().truncate().child(item.title.clone().unwrap_or_else(|| "Tool".into())))
                            .child(div().text_xs().text_color(rgb(DIM)).child(status)),
                    )
                    .when(!item.text.is_empty(), |d| {
                        d.child(
                            div()
                                .font_family(mono.clone())
                                .text_size(px(12.0))
                                .text_color(rgb(0xb0b0b0))
                                .flex()
                                .flex_col()
                                .children(limited(&item.text, 14).lines().map(|l| {
                                    let color = if l.starts_with('+') {
                                        0x9cdc9c
                                    } else if l.starts_with('-') {
                                        0xf48771
                                    } else {
                                        0xb0b0b0
                                    };
                                    div().whitespace_nowrap().overflow_hidden().text_color(rgb(color)).child(l.to_string())
                                }).collect::<Vec<_>>()),
                        )
                    })
                    .into_any_element()
            }
            "plan" => card(BORDER)
                .child(div().font_weight(FontWeight::BOLD).child("Plan"))
                .children(item.text.lines().map(|l| div().child(l.to_string())).collect::<Vec<_>>())
                .into_any_element(),
            "permission" => {
                let mut c = card(0xe5c07b)
                    .child(div().font_weight(FontWeight::BOLD).child(item.title.clone().unwrap_or_else(|| "Permission requested".into())));
                if !item.text.is_empty() {
                    c = c.child(div().font_family(mono.clone()).text_size(px(12.0)).text_color(rgb(DIM)).child(limited(&item.text, 10)));
                }
                match &item.chosen {
                    Some(choice) => {
                        let name = item.options.iter().find(|o| &o.id == choice).map(|o| o.name.clone()).unwrap_or(choice.clone());
                        c.child(div().text_color(rgb(DIM)).child(format!("→ {name}"))).into_any_element()
                    }
                    None => {
                        let mut row = div().flex().flex_row().gap_2().pt_1();
                        for (oi, o) in item.options.iter().enumerate() {
                            let allow = o.kind.starts_with("allow");
                            let params = json!({ "session": sid, "request": item.id, "option": o.id });
                            row = row.child(
                                div()
                                    .id(SharedString::from(format!("perm-{ix}-{oi}")))
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(rgb(if allow { 0x1f5f3a } else { BTN }))
                                    .hover(|d| d.bg(rgb(if allow { 0x267a4a } else { BTN_HOVER })))
                                    .child(o.name.clone())
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.chat_call("chat.permission", params.clone(), cx);
                                    })),
                            );
                        }
                        c.child(row).into_any_element()
                    }
                }
            }
            "error" => div().text_color(rgb(0xf48771)).child(item.text.clone()).into_any_element(),
            _ => div().text_xs().text_color(rgb(DIM)).child(item.text.clone()).into_any_element(),
        }
    }

    /// Inline agent picker under a "+" button: starts a session with `facts` in `cwd`.
    fn agent_picker(&self, key: &str, cwd: Option<String>, facts: Vec<FactInput>, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut row = div().flex().flex_row().flex_wrap().gap_1().items_center().child(div().text_xs().text_color(rgb(DIM)).child("new:"));
        for (i, a) in self.agents.iter().enumerate() {
            let agent = a.id.clone();
            let cwd = cwd.clone();
            let facts = facts.clone();
            row = row.child(
                div()
                    .id(SharedString::from(format!("pick-{key}-{i}")))
                    .px_2()
                    .rounded_sm()
                    .text_xs()
                    .bg(rgb(BTN))
                    .hover(|s| s.bg(rgb(BTN_HOVER)))
                    .cursor_pointer()
                    .child(a.name.clone())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.create_at = None;
                        this.new_session(agent.clone(), cwd.clone(), facts.clone(), cx);
                        cx.notify();
                    })),
            );
        }
        row.into_any_element()
    }

    fn plus_button(&self, key: String, cx: &mut Context<Self>) -> gpui::AnyElement {
        let k = key.clone();
        div()
            .id(SharedString::from(format!("plus-{key}")))
            .flex_none()
            .px_1()
            .rounded_sm()
            .text_color(rgb(DIM))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(BTN_HOVER)).text_color(rgb(TEXT)))
            .child("+")
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                this.create_at = if this.create_at.as_deref() == Some(k.as_str()) { None } else { Some(k.clone()) };
                cx.notify();
            }))
            .into_any_element()
    }

    /// Set (or clear, for an empty value) a fact as the user, via the tool path.
    fn set_user_fact(&self, session: String, key: String, value: String, cx: &mut Context<Self>) {
        let (name, args) = if value.is_empty() {
            ("fact_clear", json!({ "session": session, "key": key }))
        } else {
            ("fact_announce", json!({ "session": session, "key": key, "value": value }))
        };
        let req = json!({ "session": "", "name": name, "args": args, "idem": format!("ui-{}", tk_bus::model::now_ms()) });
        let bus = self.bus.clone();
        cx.spawn(async move |this, cx| {
            if let Err(e) = bus.request("core", "tool.call", req).await {
                let _ = this.update(cx, |wb, cx| {
                    wb.notice(format!("Could not move card: {e}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// The continuation bar (spec §4.3 `chain` views docked on top).
    fn render_chain_bar(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let v = self.views.iter().find(|v| v.view.dock == "top" && v.view.kind == "chain")?;
        let chain = v.chain.clone().unwrap_or_default();
        let ctx = self.context_session();
        let title = v.view.title.clone().unwrap_or_else(|| v.view.id.clone());
        let mut bar = div()
            .id("chain-bar")
            .h(px(CHAIN_H))
            .flex_none()
            .px_3()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .overflow_x_scroll()
            .bg(rgb(PANEL))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(div().pr_2().text_xs().font_weight(FontWeight::BOLD).text_color(rgb(DIM)).child(title.to_uppercase()));
        let Some(scope) = chain.scope.clone() else {
            let key = v.view.scope.clone().unwrap_or_default().trim_start_matches("$selected.").to_string();
            return Some(bar.child(div().text_xs().text_color(rgb(DIM)).child(format!("Select a session with a `{key}` fact to see its chain."))).into_any_element());
        };
        bar = bar.child(div().pr_2().text_xs().text_color(rgb(DIM)).child(format!("{} = {}", scope.key, scope.value)));
        for (i, step) in chain.steps.iter().enumerate() {
            if i > 0 {
                bar = bar.child(div().text_color(rgb(0x555555)).child("›"));
            }
            let sid = step.session.clone();
            let current = ctx.as_deref() == Some(step.session.as_str());
            let running = self.session(&step.session).map(|s| s.state == "running").unwrap_or(false);
            let label = step.label.clone().unwrap_or_else(|| "(no phase)".into());
            bar = bar.child(
                div()
                    .id(("chain-step", i))
                    .px_2()
                    .py(px(2.0))
                    .rounded_md()
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(if current { ACCENT } else { BORDER }))
                    .bg(rgb(if current { 0x264f78 } else { BTN }))
                    .text_color(rgb(if running { TEXT } else { DIM }))
                    .hover(|d| d.bg(rgb(BTN_HOVER)))
                    .child(label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.open_tab(&sid);
                        cx.notify();
                    })),
            );
        }
        // "+ next": a new session continuing the last step, with the chain's scope.
        let last = chain.steps.last().map(|s| s.session.clone());
        let key = format!("chain|{}", v.view.id);
        let mut facts = vec![scope.clone()];
        if let (Some(last), Some(link)) = (&last, &v.view.link_key) {
            facts.push(FactInput { key: link.clone(), value: last.clone(), kind: Some("session".into()) });
        }
        let cwd = last.as_deref().or(ctx.as_deref()).and_then(|s| self.session(s)).map(|s| s.cwd.clone());
        bar = bar.child(div().pl_2().text_xs().text_color(rgb(DIM)).child(if last.is_some() { "next" } else { "start" }));
        bar = bar.child(self.plus_button(key.clone(), cx));
        if self.create_at.as_deref() == Some(key.as_str()) {
            bar = bar.child(div().pl_2().child(self.agent_picker(&key, cwd, facts, cx)));
        }
        Some(bar.into_any_element())
    }

    /// A board view (spec §4.3): columns by a fact, cards are sessions, "+" and drag.
    fn render_board(&self, pi: usize, uri: &str, cx: &mut Context<Self>) -> gpui::AnyElement {
        let view_id = uri.trim_start_matches(VIEW_PREFIX);
        let Some(v) = self.views.iter().find(|v| v.view.id == view_id) else {
            return div().p_4().text_color(rgb(DIM)).child(format!("View '{view_id}' is not configured.")).into_any_element();
        };
        let board = v.board.clone().unwrap_or_default();
        let column_key = v.view.column_key.clone().unwrap_or_default();
        let ctx = self.context_session();
        let mut head = div()
            .flex()
            .flex_row()
            .gap_3()
            .items_center()
            .pb_3()
            .child(div().text_size(px(18.0)).font_weight(FontWeight::BOLD).child(v.view.title.clone().unwrap_or_else(|| view_id.to_string())))
            .child(div().text_xs().text_color(rgb(DIM)).child(format!("columns by `{column_key}`")));
        for f in &board.filter {
            head = head.child(div().px_2().rounded_sm().text_xs().bg(rgb(BTN)).child(format!("{} = {}", f.key, f.value)));
        }
        for k in &board.unresolved {
            head = head.child(div().text_xs().text_color(rgb(0xe5c07b)).child(format!("select a session with a `{k}` fact to filter")));
        }
        let mut cols = div().flex_1().min_h_0().flex().flex_row().gap_3();
        for (ci, col) in board.columns.iter().enumerate() {
            let value = col.value.clone();
            let label = if value.is_empty() { "(none)".to_string() } else { value.clone() };
            let key = format!("board|{view_id}|{value}");
            let mut facts = board.filter.clone();
            if !value.is_empty() {
                facts.push(FactInput { key: column_key.clone(), value: value.clone(), kind: None });
            }
            let cwd = col.sessions.first().or(ctx.as_ref()).and_then(|s| self.session(s)).map(|s| s.cwd.clone());
            let drop_value = value.clone();
            let drop_key = column_key.clone();
            let mut column = div()
                .id(("board-col", ci))
                .flex_1()
                .min_w(px(200.0))
                .h_full()
                .p_2()
                .rounded_md()
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(BORDER))
                .flex()
                .flex_col()
                .gap_2()
                .overflow_y_scroll()
                .when(v.view.drag, |d| {
                    d.drag_over::<CardDrag>(|style, _, _, _| style.border_color(rgb(ACCENT)).bg(rgb(0x1f2a36)))
                        .on_drop(cx.listener(move |this, card: &CardDrag, _, cx| {
                            this.set_user_fact(card.session.clone(), drop_key.clone(), drop_value.clone(), cx);
                        }))
                })
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().font_weight(FontWeight::BOLD).child(label))
                        .child(div().text_xs().text_color(rgb(DIM)).child(col.sessions.len().to_string()))
                        .when(v.view.create, |d| d.child(self.plus_button(key.clone(), cx))),
                );
            if self.create_at.as_deref() == Some(key.as_str()) {
                column = column.child(self.agent_picker(&key, cwd, facts, cx));
            }
            for (k, sid) in col.sessions.iter().enumerate() {
                let Some(s) = self.session(sid) else { continue };
                let open = sid.clone();
                let selected = ctx.as_deref() == Some(sid.as_str());
                let title = self.session_title(sid);
                let drag = CardDrag { session: sid.clone(), title: title.clone() };
                let running = s.state == "running";
                column = column.child(
                    div()
                        .id(SharedString::from(format!("card-{pi}-{ci}-{k}")))
                        .p_2()
                        .rounded_md()
                        .bg(rgb(if selected { 0x264f78 } else { TERM_BG }))
                        .border_1()
                        .border_color(rgb(if selected { ACCENT } else { BORDER }))
                        .cursor_pointer()
                        .hover(|d| d.border_color(rgb(0x555555)))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap_2()
                                .items_center()
                                .child(div().text_color(rgb(self.kind_color(sid))).child(self.kind_marker(sid)))
                                .child(div().flex_1().min_w_0().truncate().text_color(rgb(if running { TEXT } else { DIM })).child(title)),
                        )
                        .child(div().text_xs().text_color(rgb(DIM)).truncate().child(format!("{} · {}", s.agent, s.cwd)))
                        .when(v.view.drag, |d| d.on_drag(drag, |card: &CardDrag, _, _, cx| cx.new(|_| card.clone())))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.open_tab(&open);
                            cx.notify();
                        })),
                );
            }
            cols = cols.child(column);
        }
        div()
            .flex_1()
            .min_h_0()
            .p_4()
            .flex()
            .flex_col()
            .bg(rgb(TERM_BG))
            .child(head)
            .child(cols)
            .into_any_element()
    }

    fn render_doc_list(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.active_docs.is_empty() {
            return div().px_3().text_color(rgb(DIM)).child("none").into_any_element();
        }
        let open = self.active_tab();
        div()
            .flex()
            .flex_col()
            .children(self.active_docs.iter().enumerate().map(|(i, d)| {
                let uri = d.uri.clone();
                let selected = open.as_deref() == Some(d.uri.as_str());
                div()
                    .id(("doc", i))
                    .px_3()
                    .py(px(3.0))
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .when(selected, |x| x.bg(rgb(BTN_HOVER)))
                    .hover(|x| x.bg(rgb(BTN)))
                    .child(div().flex_none().text_xs().text_color(rgb(DIM)).child(match d.kind.as_str() {
                        "changeset" => "±",
                        "markdown" => "¶",
                        "decision-list" => "⚖",
                        _ => "•",
                    }))
                    .child(div().flex_1().min_w_0().truncate().child(d.title.clone()))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.open_tab(&uri);
                        this.fetch_doc(uri.clone(), cx);
                        cx.notify();
                    }))
            }))
            .into_any_element()
    }

    fn render_document(&self, pi: usize, uri: &str, cx: &mut Context<Self>) -> gpui::AnyElement {
        let body: gpui::AnyElement = match (self.docs.get(uri), self.doc_errors.get(uri)) {
            (_, Some(err)) => div().text_color(rgb(0xf48771)).child(err.clone()).into_any_element(),
            (None, None) => div().text_color(rgb(DIM)).child("loading…").into_any_element(),
            (Some(doc), None) => match doc.kind.as_str() {
                "markdown" => markdown::render(doc.body["text"].as_str().unwrap_or(""), self.font_family.clone()),
                "changeset" => self.render_changeset(doc, cx),
                "decision-list" => render_decisions(doc),
                other => div().text_color(rgb(DIM)).child(format!("no renderer for document kind '{other}'")).into_any_element(),
            },
        };
        div()
            .id(SharedString::from(format!("doc-{pi}")))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .bg(rgb(TERM_BG))
            .p_4()
            .child(body)
            .into_any_element()
    }

    fn render_changeset(&self, doc: &Document, cx: &mut Context<Self>) -> gpui::AnyElement {
        const MAX_LINES_PER_FILE: usize = 400;
        let b = &doc.body;
        let files = b["files"].as_array().cloned().unwrap_or_default();
        let (added, removed) = files.iter().fold((0, 0), |(a, r), f| {
            (a + f["added"].as_u64().unwrap_or(0), r + f["removed"].as_u64().unwrap_or(0))
        });
        let many = files.len() > 8;
        let mut col = div().flex().flex_col().gap_1().child(
            div()
                .flex()
                .flex_row()
                .gap_3()
                .items_center()
                .pb_2()
                .child(div().text_size(px(18.0)).font_weight(FontWeight::BOLD).child(doc.title.clone()))
                .child(div().text_color(rgb(DIM)).child(format!("{} files", files.len())))
                .child(div().text_color(rgb(0x23d18b)).child(format!("+{added}")))
                .child(div().text_color(rgb(0xf14c4c)).child(format!("−{removed}")))
                .child(div().text_xs().text_color(rgb(DIM)).child(b["root"].as_str().unwrap_or("").to_string())),
        );
        if files.is_empty() {
            col = col.child(div().text_color(rgb(DIM)).child("No uncommitted changes."));
        }
        if b["truncated"].as_bool() == Some(true) {
            col = col.child(div().text_xs().text_color(rgb(0xe5e510)).child("Diff truncated (very large)."));
        }
        for (i, f) in files.iter().enumerate() {
            let path = f["path"].as_str().unwrap_or("").to_string();
            let key = format!("{}#{}", doc.uri, path);
            // Large changesets start collapsed; the set records the user's toggles.
            let collapsed = self.collapsed_files.contains(&key) != many;
            let status = f["status"].as_str().unwrap_or("M").to_string();
            let status_color = match status.as_str() {
                "A" | "?" => 0x23d18b,
                "D" => 0xf14c4c,
                _ => 0xe5c07b,
            };
            let toggle = key.clone();
            col = col.child(
                div()
                    .id(("file", i))
                    .mt_2()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(rgb(PANEL))
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .hover(|x| x.bg(rgb(BTN)))
                    .child(div().w(px(12.0)).text_xs().text_color(rgb(DIM)).child(if collapsed { "▸" } else { "▾" }))
                    .child(div().w(px(14.0)).text_color(rgb(status_color)).child(status))
                    .child(div().flex_1().min_w_0().truncate().child(path.clone()))
                    .child(div().text_xs().text_color(rgb(0x23d18b)).child(format!("+{}", f["added"].as_u64().unwrap_or(0))))
                    .child(div().text_xs().text_color(rgb(0xf14c4c)).child(format!("−{}", f["removed"].as_u64().unwrap_or(0))))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if !this.collapsed_files.remove(&toggle) {
                            this.collapsed_files.insert(toggle.clone());
                        }
                        cx.notify();
                    })),
            );
            if !collapsed {
                let diff = f["diff"].as_str().unwrap_or("");
                let lines: Vec<&str> = diff.lines().collect();
                let shown = lines.len().min(MAX_LINES_PER_FILE);
                let mut block = div()
                    .flex()
                    .flex_col()
                    .font_family(self.font_family.clone())
                    .text_size(px(13.0))
                    .line_height(px(18.0));
                for l in &lines[..shown] {
                    let (fg, bg) = if l.starts_with("@@") {
                        (0x569cd6, None)
                    } else if l.starts_with('+') {
                        (0x9cdc9c, Some(0x1f3a26))
                    } else if l.starts_with('-') {
                        (0xf48771, Some(0x4a1f22))
                    } else {
                        (0xb0b0b0, None)
                    };
                    let mut row = div().px_2().whitespace_nowrap().overflow_hidden().text_color(rgb(fg)).child(l.replace('\t', "    "));
                    if let Some(bg) = bg {
                        row = row.bg(rgb(bg));
                    }
                    block = block.child(row);
                }
                if lines.len() > shown {
                    block = block.child(div().px_2().text_color(rgb(DIM)).child(format!("… {} more lines", lines.len() - shown)));
                }
                col = col.child(block);
            }
        }
        col.into_any_element()
    }

    fn render_pane(&self, pi: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = &self.panes[pi];
        let is_active = pi == self.active_pane;
        let tab_bar = div()
            .id(SharedString::from(format!("tabbar-{pi}")))
            .overflow_x_scroll()
            .h(px(TAB_H))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .bg(rgb(PANEL))
            .border_b_1()
            .border_color(rgb(if is_active { ACCENT } else { BORDER }))
            .children(pane.tabs.iter().enumerate().map(|(ti, sid)| {
                let active = ti == pane.active;
                let ended = self.session(sid).map(|s| s.state != "running").unwrap_or(false);
                // The active tab merges into the content (same background), carries an accent
                // strip on top and bright text; inactive tabs are darker with dim text.
                let strip = match (active, is_active) {
                    (true, true) => ACCENT,
                    (true, false) => 0x666666,
                    _ => TAB_INACTIVE,
                };
                let text = match (active, ended) {
                    (_, true) => 0x666666,
                    (true, false) => 0xffffff,
                    (false, false) => DIM,
                };
                div()
                    .id(SharedString::from(format!("tab-{pi}-{ti}")))
                    .h_full()
                    .flex()
                    .flex_col()
                    .cursor_pointer()
                    .border_r_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(if active { TERM_BG } else { TAB_INACTIVE }))
                    .text_color(rgb(text))
                    .when(!active, |d| d.hover(|s| s.bg(rgb(BTN))))
                    .child(div().h(px(2.0)).w_full().flex_none().bg(rgb(strip)))
                    .child(
                        div()
                            .flex_1()
                            .px_3()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(div().text_color(rgb(if active { self.kind_color(sid) } else { 0x666666 })).child(self.kind_marker(sid)))
                            .child(self.session_title(sid))
                            .child(
                        div()
                            .id(SharedString::from(format!("close-{pi}-{ti}")))
                            .px_1()
                            .rounded_sm()
                            .text_color(rgb(DIM))
                            .hover(|s| s.bg(rgb(BTN_HOVER)).text_color(rgb(TEXT)))
                            .child("×")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                this.close_tab(pi, ti);
                                cx.notify();
                            })),
                            ),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if let Some(p) = this.panes.get_mut(pi) {
                            p.active = ti;
                        }
                        this.active_pane = pi;
                        this.want_focus = true;
                        cx.notify();
                    }))
            }))
            .child(div().flex_1())
            .child(
                div()
                    .id(SharedString::from(format!("split-{pi}")))
                    .px_2()
                    .mx_1()
                    .rounded_sm()
                    .text_xs()
                    .text_color(rgb(DIM))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(BTN_HOVER)).text_color(rgb(TEXT)))
                    .child("Split ▸")
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.split(pi, cx);
                        cx.notify();
                    })),
            );

        let content = match pane.tabs.get(pane.active) {
            Some(uri) if is_doc(uri) => self.render_document(pi, uri, cx),
            Some(uri) if is_view(uri) => self.render_board(pi, uri, cx),
            Some(sid) if self.is_chat(sid) => self.render_chat(pi, sid, cx),
            Some(sid) => self.render_terminal(pi, sid, cx).into_any_element(),
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(DIM))
                .child("No tab open. Start or open a session from the left.")
                .into_any_element(),
        };

        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(tab_bar)
            .child(content)
    }

    fn render_terminal(&self, pi: usize, sid: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.panes[pi].focus.clone();
        let focus2 = focus.clone();
        let sid_key = sid.to_string();
        let sid_scroll = sid.to_string();
        let cell_w = self.cell_w;
        let line_h = self.line_h;
        let ended = self.session(sid).map(|s| s.state != "running").unwrap_or(false);
        let lines: Vec<gpui::AnyElement> = match self.frames.get(sid) {
            Some(frame) => frame.lines.iter().map(|l| termdraw::render_line(l, cell_w, line_h, TERM_BG)).collect(),
            None => vec![div().text_color(rgb(DIM)).child("connecting…").into_any_element()],
        };
        div()
            .id(SharedString::from(format!("term-{pi}")))
            .track_focus(&focus)
            .flex_1()
            .min_h_0()
            .p(px(PAD))
            .bg(rgb(TERM_BG))
            .overflow_hidden()
            .font_family(self.font_family.clone())
            .text_size(px(self.font_size))
            .line_height(px(line_h))
            .text_color(rgb(0xd4d4d4))
            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
                cx.stop_propagation();
                this.on_key(&sid_key, ev, cx);
            }))
            .on_scroll_wheel(cx.listener(move |this, ev: &ScrollWheelEvent, _, _| this.on_scroll(&sid_scroll, ev)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.active_pane = pi;
                    window.focus(&focus2);
                    cx.notify();
                }),
            )
            .children(lines)
            .when(ended, |d| d.child(div().text_color(rgb(DIM)).child("[session ended]")))
    }
}

fn section_header(text: &str) -> impl IntoElement {
    div()
        .px_3()
        .pt_3()
        .pb_1()
        .text_xs()
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(DIM))
        .child(text.to_string())
}

/// The decision log as cards (spec §7.1): title, status, tags, decision, why, alternatives,
/// context. Project decisions are files in `.tk/decisions/`, so the file name is shown too.
fn render_decisions(doc: &Document) -> gpui::AnyElement {
    let entries = doc.body["entries"].as_array().cloned().unwrap_or_default();
    let dir = doc.body["dir"].as_str().unwrap_or("").to_string();
    let mut col = div()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .flex()
                .flex_row()
                .gap_3()
                .items_center()
                .child(div().text_size(px(18.0)).font_weight(FontWeight::BOLD).child(doc.title.clone()))
                .child(div().text_xs().text_color(rgb(DIM)).child(dir)),
        );
    if entries.is_empty() {
        col = col.child(div().text_color(rgb(DIM)).child("No decisions yet. Agents record them with decision_log."));
    }
    for e in entries {
        let s = |k: &str| e[k].as_str().filter(|v| !v.is_empty()).map(String::from);
        let superseded = s("status").as_deref() == Some("superseded");
        let meta = [s("source"), s("date").map(|d| d.replace('T', " ").replace('Z', " UTC")), s("file")]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        let mut tags = div().flex().flex_row().flex_wrap().gap_1();
        if let Some(t) = e["tags"].as_object() {
            for (k, v) in t {
                tags = tags.child(div().px_2().rounded_sm().text_xs().bg(rgb(BTN)).child(format!("{k} = {}", v.as_str().unwrap_or(""))));
            }
        }
        let mut card = div()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .flex()
            .flex_col()
            .gap_1()
            .when(superseded, |d| d.opacity(0.55))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().font_weight(FontWeight::BOLD).child(s("title").unwrap_or_default()))
                    .child(
                        div()
                            .px_2()
                            .rounded_sm()
                            .text_xs()
                            .bg(rgb(if superseded { 0x4a3a1f } else { 0x1f3a26 }))
                            .text_color(rgb(if superseded { 0xe5c07b } else { 0x9cdc9c }))
                            .child(s("status").unwrap_or_else(|| "accepted".into())),
                    ),
            )
            .child(div().text_xs().text_color(rgb(DIM)).child(meta))
            .child(tags);
        if let Some(by) = s("superseded_by") {
            card = card.child(div().text_xs().text_color(rgb(0xe5c07b)).child(format!("superseded by {by}")));
        }
        for (label, key) in [("Decision", "decision"), ("Why", "rationale"), ("Alternatives", "alternatives"), ("Context", "context")] {
            if let Some(v) = s(key) {
                card = card.child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(div().w(px(90.0)).flex_none().text_color(rgb(DIM)).child(label))
                        .child(div().flex_1().min_w_0().child(v)),
                );
            }
        }
        col = col.child(card);
    }
    col.into_any_element()
}

impl Render for Workbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_sizes(window);
        self.sync_active_facts(cx);
        let chat_tabs: Vec<String> =
            self.panes.iter().filter_map(|p| p.tabs.get(p.active)).filter(|t| self.is_chat(t)).cloned().collect();
        for sid in chat_tabs {
            self.chat_scroll.entry(sid.clone()).or_default();
            self.ensure_chat(&sid, cx);
        }
        for uri in self.visible_docs() {
            if !self.docs.contains_key(&uri) && !self.doc_errors.contains_key(&uri) {
                self.fetch_doc(uri, cx);
            }
        }
        if self.want_focus {
            self.want_focus = false;
            if let Some(p) = self.panes.get(self.active_pane) {
                window.focus(&p.focus);
            }
        }
        let panes: Vec<gpui::AnyElement> =
            (0..self.panes.len()).map(|pi| self.render_pane(pi, cx).into_any_element()).collect();
        let status = self.notices.last().cloned().unwrap_or_default();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .text_sm()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(self.render_left(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .flex_col()
                            .children(self.render_chain_bar(cx))
                            .child(div().flex_1().min_h_0().flex().flex_row().children(panes)),
                    )
                    .child(self.render_right(cx)),
            )
            .child(
                div()
                    .h(px(STATUS_H))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_4()
                    .px_3()
                    .bg(rgb(PANEL))
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .text_xs()
                    .child(
                        div()
                            .text_color(rgb(if self.connected { 0x23d18b } else { 0xe5e510 }))
                            .child(if self.connected { "● connected" } else { "○ reconnecting…" }),
                    )
                    .child(div().text_color(rgb(DIM)).child(status)),
            )
    }
}
