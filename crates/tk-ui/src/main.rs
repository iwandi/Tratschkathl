//! L4 UI: the multi-document workbench (spec §4.5) built on GPUI. Holds no domain state:
//! sessions come from Services, terminal frames from the Agent Handler. The layout is handed
//! to the next UI version on reload.

use std::collections::HashMap;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    div, font, px, rgb, App, Application, Bounds, ClickEvent, Context, FocusHandle, FontWeight, KeyDownEvent,
    MouseButton, MouseDownEvent, PathPromptOptions, ScrollWheelEvent, SharedString, TitlebarOptions, Window,
    WindowBounds, WindowOptions,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use tk_bus::frame::{Frame, KeyInput, Run, BOLD, ITALIC, UNDERLINE};
use tk_bus::model::{AgentConfig, SessionInfo};
use tk_bus::settings::Settings;
use tk_bus::{client::handler, Bus};

const LEFT_W: f32 = 250.0;
const RIGHT_W: f32 = 250.0;
const STATUS_H: f32 = 26.0;
const TAB_H: f32 = 32.0;
const PAD: f32 = 6.0;

const BG: u32 = 0x181818;
const PANEL: u32 = 0x202020;
const BORDER: u32 = 0x333333;
const TEXT: u32 = 0xcccccc;
const DIM: u32 = 0x888888;
const ACCENT: u32 = 0x3b8eea;
const BTN: u32 = 0x2d2d2d;
const BTN_HOVER: u32 = 0x3a3a3a;
const TAB_ACTIVE: u32 = 0x1e1e1e;
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
            vec!["term.".into(), "sessions.".into(), "host.".into(), "layer.".into(), "workbench.".into()],
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
                cx.notify();
            });
            let _ = bus2.request("agents", "term.refresh", json!({})).await;
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
        self.want_focus = true;
    }

    fn session(&self, id: &str) -> Option<&SessionInfo> {
        self.sessions.iter().find(|s| s.id == id)
    }

    fn session_title(&self, id: &str) -> String {
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
            "term.ended" | "sessions.changed" => self.refresh_sessions(cx),
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
                Some("services") => self.refresh_sessions(cx),
                _ => {}
            },
            _ => {}
        }
        cx.notify();
    }

    fn new_session(&mut self, agent: String, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose the working folder".into()),
        });
        let bus = self.bus.clone();
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(dir) = paths.into_iter().next() else { return };
            let res = bus
                .request("agents", "session.create", json!({ "agent": agent, "cwd": dir.to_string_lossy() }))
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
        let pane_h = h - STATUS_H - TAB_H - 2.0 * PAD - 2.0;
        let cols = (pane_w / self.cell_w).floor().max(10.0) as u16;
        let rows = (pane_h / self.line_h).floor().max(4.0) as u16;
        for p in &self.panes {
            if let Some(sid) = p.tabs.get(p.active) {
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
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.new_session(id.clone(), cx)))
            }))
            .child(section_header("SESSIONS"))
            .child(
                div()
                    .id("session-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .children(self.sessions.iter().enumerate().map(|(i, s)| {
                        let id = s.id.clone();
                        let running = s.state == "running";
                        let selected = active_sid.as_deref() == Some(s.id.as_str());
                        div()
                            .id(("session", i))
                            .px_3()
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
                    })),
            )
    }

    fn render_right(&self) -> impl IntoElement {
        let active = self
            .panes
            .get(self.active_pane)
            .and_then(|p| p.tabs.get(p.active))
            .and_then(|sid| self.session(sid));
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
                    .child(div().px_3().text_color(rgb(DIM)).child("none yet (M1)"))
                    .child(section_header("DOCUMENTS"))
                    .child(div().px_3().text_color(rgb(DIM)).child("none yet (M2)"));
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

    fn render_pane(&self, pi: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = &self.panes[pi];
        let is_active = pi == self.active_pane;
        let tab_bar = div()
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
                div()
                    .id(SharedString::from(format!("tab-{pi}-{ti}")))
                    .h_full()
                    .px_3()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .border_r_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(if active { TAB_ACTIVE } else { PANEL }))
                    .text_color(rgb(if ended { DIM } else { TEXT }))
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
            Some(frame) => frame.lines.iter().map(|l| render_line(l, cell_w, line_h).into_any_element()).collect(),
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

fn render_line(line: &[Run], cell_w: f32, line_h: f32) -> impl IntoElement {
    div().h(px(line_h)).flex().flex_row().whitespace_nowrap().children(line.iter().map(move |r| {
        let cells = if r.cells > 0 { r.cells as f32 } else { r.text.chars().count() as f32 };
        let mut d = div()
            .flex_none()
            .w(px(cells * cell_w))
            .h(px(line_h))
            .overflow_hidden()
            .text_color(rgb(r.fg))
            .child(r.text.clone());
        if r.bg != TERM_BG {
            d = d.bg(rgb(r.bg));
        }
        if r.flags & BOLD != 0 {
            d = d.font_weight(FontWeight::BOLD);
        }
        if r.flags & ITALIC != 0 {
            d = d.italic();
        }
        if r.flags & UNDERLINE != 0 {
            d = d.underline();
        }
        d
    }))
}

impl Render for Workbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_sizes(window);
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
                    .child(div().flex_1().min_w_0().h_full().flex().flex_row().children(panes))
                    .child(self.render_right()),
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
