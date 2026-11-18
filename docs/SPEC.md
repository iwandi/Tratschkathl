# Tratschkathl — Spec v0.2

> *Tratschkathl* (Bavarian/Austrian): the neighbour who knows everybody's business and who is connected to whom.
> That is the job: know what every agent session is about, and connect the ones that belong together.

Tratschkathl is a desktop orchestrator for LLM agent sessions. It hosts sessions, collects **facts** that
agents and plugins announce about them, and uses those facts to sort, filter, connect and show sessions,
along with the documents that belong to them.

The app is **self-hosting**: from milestone M0 on, we develop Tratschkathl with agents running inside Tratschkathl.
The app can rebuild and reload itself layer by layer at safe points, and the sessions survive the reload.

The app is **agent-centric**. The user starts with a minimal surface: sessions, chat, terminal, approvals and basic settings.
Every other view, button and command exists because an agent configured it.

---

## 1. Goals and non-goals

**Goals**

- One window for many agent sessions: chat sessions (ACP) and terminal sessions (Claude Code in a PTY).
- Sessions are correlated by **facts** (project, branch, issue, phase, …) and not by folders or tabs.
- Views (tree, board, continuation bar) are defined in config, and agents can edit that config.
- Plugins define capability layers (VCS, forge, build, work items, agents, documents).
- Shared services for agents: decision log, automation, resource management.
- Self-reload per layer (Core, Services, Agent Handler, UI) with rollback, so the app can be developed safely from inside itself.
- Agent-centric UI: a minimal fixed user surface, and agents configure the rest (§6.1).

**Non-goals (for now)**

- Running as a multi-user server. It is a local desktop app.
- Being an IDE. Editing happens in the agents and in the user's editor; we show and link.
- Running third-party plugins in a sandbox (see §10, Open questions).

---

## 2. Tech stack

| Concern            | Choice                                                                 |
|--------------------|------------------------------------------------------------------------|
| Language           | Rust (Cargo workspace)                                                 |
| GUI                | GPUI (pinned Zed commit or crates.io release)                          |
| Agent protocol     | ACP (Agent Client Protocol, JSON-RPC over stdio), `agent-client-protocol` crate |
| Claude via ACP     | Claude Code ACP adapter (external process)                             |
| Terminal           | `portable-pty` (ConPTY on Windows) + `alacritty_terminal` for VT state, rendered with GPUI |
| Tools for agents   | ACP tools for ACP-hosted agents; MCP (`rmcp`) through a per-session stdio bridge for terminal agents (§4.1) |
| IPC between layers | Named pipes (Windows) / unix sockets, versioned interfaces             |
| Storage            | SQLite (WAL): an append-only event log plus projections                |
| Config             | TOML, schema-validated, hot-reloaded                                   |
| Primary platform   | Windows; keep macOS and Linux possible                                 |

---

## 3. Process architecture and reload layers

The agent that edits Tratschkathl runs inside Tratschkathl. Reloading any part of the app must therefore not kill an
agent, its terminal or its pending tool call.

The app is split into **four reload layers**. Each layer is its own OS process and can be reloaded on its own.
Three small helper executables sit next to the layers. They are never reloaded in place.

```
 tk-launcher (external) ──reboots / rolls back──►  L1 Core
                                                     │  bus · layer supervisor · builds · versions · handoff
                       ┌─────────────────────────────┼────────────────────────────┐
                       ▼                             ▼                            ▼
                 L2 Services                  L3 Agent Handler                  L4 UI
                 facts, sessions, views,      ACP client, terminal emulation,   GPUI: views, chat,
                 docs, decisions, automation, session lifecycle,                terminal rendering,
                 resources, plugins,          agent plugins (Claude)            document renderers
                 MCP tool implementations            │
                                                     │ attaches to recorded streams
 per session:   tk-shim ── owns PTY / stdio, records streams ──► agent process (claude, ACP adapter)
                                                                   └─► tk-mcp-bridge (stdio MCP) ──► Core ──► Services
```

### 3.1 Layers

| Layer                | Process       | Owns                                                                                          | Reloaded by          | Expected change rate |
|----------------------|---------------|-----------------------------------------------------------------------------------------------|----------------------|----------------------|
| **L1 Core**          | `tk-core`     | Message bus, layer supervisor, build + stage + self-test, version manager, request queue, handoff store, shim registry, `core.db` | `tk-launcher` (external) | rare |
| **L2 Services**      | `tk-services` | Orchestrator domain: facts, sessions, views, documents, decisions, automation, resources, non-agent plugins, MCP tool implementations, `tratschkathl.db` | Core | often |
| **L3 Agent Handler** | `tk-agents`   | ACP client, terminal emulation (VT state via `alacritty_terminal`), agent plugins (Claude), session lifecycle (start, resume, cancel), MCP config injection | Core | medium |
| **L4 UI**            | `tk-ui`       | GPUI workbench (windows, docks, panes, tabs), navigator views, chat and terminal rendering, document renderers. **No domain state** | Core | very often |

- Dependencies: UI → Services + Agent Handler, Agent Handler → Services + Core, and Services → Core.
  All of these calls go over Core's bus.
- Layers do **not** die with Core. When Core restarts, the other layers reconnect to the new Core (with backoff), and the new
  Core adopts them by pid and version.
- Terminal emulation runs in the Agent Handler, which sends grid snapshots and damage diffs to the UI. That way a UI reload
  is instant and does not need a replay.

### 3.2 Fixed helpers

The helpers are small on purpose, so they rarely change. A new helper version is used by sessions started after it was
installed. Running sessions keep the version they started with.

- **`tk-launcher`** — the external process. It starts Core, health-checks it, reboots it into a new version and rolls it back
  if needed. It updates itself on the next app start.
- **`tk-shim`** — one per agent session, similar to `dtach`.
  - It owns the PTY (ConPTY) or the agent's stdio pipes, and it keeps the agent alive.
  - It records every stream: a full log of ACP's newline-delimited JSON, and a ring buffer of the last N MB of PTY output.
  - It serves the streams over a named pipe, starting from any offset.
  - Because the shim holds the session, **every** layer, Core included, can restart without killing an agent.
- **`tk-mcp-bridge`** — a stdio MCP server that each **terminal** agent starts as its own child process. Claude Code gets it
  through `--mcp-config`. ACP agents get their tools over ACP (§4.1). For them, the bridge is only a fallback for an agent that
  cannot take tools over ACP.
  - It carries the session token and handles the MCP handshake itself.
  - It turns each `tools/call` into a bus request with an idempotency id.
  - While Core or Services restart, it holds the call and retries it, up to a timeout. The agent sees a slow tool call,
    not a failed one.
  - After a Services reload it sends `notifications/tools/list_changed`.

### 3.3 What survives a reload

| Reloaded             | Agent processes      | Terminal screen                                   | Chat transcript     | Pending MCP call                      | UI state            |
|----------------------|----------------------|---------------------------------------------------|---------------------|---------------------------------------|---------------------|
| UI                   | ✓                    | ✓                                                 | ✓                   | ✓                                     | restored from handoff |
| Agent Handler        | ✓ (held by the shim) | rebuilt from the shim ring buffer + a resize to force a redraw | rebuilt from the shim log | MCP: ✓ (goes through the bridge). ACP tool calls: buffered by the shim, answered by the new Agent Handler (handoff) | ✓ |
| Services             | ✓                    | ✓                                                 | ✓                   | held by Core or the Agent Handler, delivered to the new Services | ✓ |
| Core                 | ✓                    | ✓                                                 | ✓                   | held by the bridge, re-sent to the new Core | ✓ (layers reconnect) |
| Full restart (reboot, helper upgrade) | resumed with `claude --resume` / ACP `session/load` | replayed from the log | ✓ | lost; the agent retries | restored |

### 3.4 Handoff

Before a layer exits for a reload, it writes a versioned **handoff blob** to Core. Its successor receives the blob when it starts.

- **UI:** the workbench layout (windows, docks, panes, tabs), the selected view, scroll positions and unsent drafts.
- **Agent Handler:** the stream offset for each session, pending ACP requests to the agent, and agent requests that are
  waiting for an answer from the client (ACP tool calls, `fs/*`, permission prompts).
- **Services:** in-flight automation runs. Ideally this is empty, because domain state lives in the DB.
- **Core:** bus registrations, queued requests and running build jobs. Core writes these to `core.db` for its successor.

If the successor cannot read the blob, for example because its version is incompatible, it starts cold from persistent
state, and the failure is logged.

### 3.5 Reload flow

1. An agent calls `host_reload(layers = auto | [..], reason)`, or the user runs a reload command that an agent configured (§6.1).
2. Core builds the workspace.
   - `auto` reloads every layer whose binary changed.
   - Each binary is staged in its own folder, `versions/<layer>/<build-id>/`, because Windows locks running executables.
3. Each new binary runs `--self-test` headless. The test loads the config, opens the DB read-only, checks plugin
   registration and checks interface versions. If any self-test fails, the reload is aborted, the errors are returned to the
   caller, and nothing changes.
4. **Compatibility check.** Every interface between layers has a `major.minor` version. If a new binary breaks an interface
   that a running layer uses, that layer joins the **reload set**. If that layer was not rebuilt to match, the reload is refused.
5. **Safe point** for each layer:
   - UI: any time.
   - Agent Handler: at a frame boundary. The shim buffers the rest.
   - Services: no open transaction, and no plugin operation marked non-interruptible.
   - Core: no build job writing.
6. The swap goes bottom-up: Core → Services → Agent Handler → UI. For each layer: write the handoff, stop, start the new
   version, wait for it to report healthy, then move to the next layer.
7. A layer is rolled back to its **last known good** version if it is not healthy within N seconds, or if it crashes three times
   in its first minute. The launcher rolls back Core; Core rolls back the other layers.
8. `host_reload` returns the result: the version of each layer, the self-test output, and success or rollback.
   - If Core itself is reloaded, the bridge keeps the call open. The new Core finishes the job recorded in `core.db` and
     sends the answer.
   - Every reload is written to the decision log.

The agent does not have to end its turn before a reload: because of the shim and the bridge, it keeps running throughout.

---

## 4. Domain model

### 4.1 Session

```
Session { id, kind: Chat | Terminal, agent: AgentRef, title, created_at, state: Running | Idle | Waiting | Ended,
          cwd, origin: User | Automation(rule_id) | Agent(session_id) }
```

- **Chat session**: an interaction similar to Claude Desktop, driven over ACP. It shows messages, tool calls, diffs,
  plans and permission prompts.
- **Terminal session**: a terminal emulator that hosts a TUI agent such as Claude Code.
- Every session runs under its own `tk-shim`, which keeps the agent alive across reloads.
- Every tool call is tied to the session that made it, through the ACP session id or a **per-session token** in the bridge.

**How the orchestrator tools (§6) reach the agent:**

| Session kind          | Tool delivery                                                                                          | Answered by               |
|-----------------------|--------------------------------------------------------------------------------------------------------|---------------------------|
| Chat (agent over ACP) | **ACP tools** on the ACP connection itself (MCP-over-ACP / ACP extension methods, whichever the agent supports). Fallback: `tk-mcp-bridge` in `session/new { mcpServers }` | Agent Handler → Services |
| Terminal (Claude Code) | MCP through `tk-mcp-bridge` (`--mcp-config`, generated per session)                                  | Core → Services           |

For ACP agents, the Agent Handler also implements ACP's **client-side capabilities** itself:

- `fs/read_text_file` and `fs/write_text_file`: the orchestrator sees every file the agent reads and writes.
  This feeds fact derivation and the change visualization (§7.4).
- `terminal/*`: commands the agent runs are started under a shim, and they show up as terminals the user can watch.
- `session/request_permission`: routed to the approval prompts in the baseline UI (§6.1).

### 4.2 Facts — the core concept

A fact is a statement about a session that the system can match on. Agents announce and clear facts with MCP tools.
Plugins can set facts too. Users set facts only through commands that an agent configured (§6.1).

```
Fact { id, session_id, key, value: FactValue, source: Agent | Plugin(id) | User | Automation,
       note?, announced_at, cleared_at? }

FactValue = Text(String) | Ref(kind, id)        // e.g. Ref("jira", "PROJ-123"), Ref("session", "s_42"), Ref("git.branch", "feat/x")
```

- Facts are append-only events. "Current facts" are the announced facts that have not been cleared. History is never lost,
  so the change visualization (§7.4) can replay it.
- **Key schema** (in config) sets each key's cardinality and value type:
  - `single`: announcing a new value replaces the old one (e.g. `phase`).
  - `multi`: several values can exist at the same time (e.g. `issue`).
- `Ref` values can be resolved by plugins into a title, URL, status and documents. This is how facts and plugins connect.
- Well-known keys (extensible): `project`, `repo`, `branch`, `worktree`, `changelist`, `issue`, `review`, `build`,
  `phase`, `continues` (a ref to the previous session in a chain), `topic`.
- Plugins can **derive** facts. For example, the Git plugin sets `repo`, `branch` and `worktree` from a session's `cwd`.

**Correlation** means sessions that share facts. "All sessions on `branch=feat/x`" and
"the session chain of `issue=PROJ-123`" are both just fact queries.

### 4.3 Views

Views are projections of sessions, grouped by facts. They are defined in config, and agents can edit them
(`view_config_get` / `view_config_set`, schema-validated and versioned, so they can be rolled back).

```toml
[[view]]
id = "selector"
type = "tree"
dock = "left"                                    # where the view lives in the workbench (§4.5)
levels = ["project", "branch", "issue"]          # nesting by fact keys
show_unassigned = true

[[view]]
id = "board"
type = "board"
column_key = "phase"
columns = ["concept", "designed", "implementing", "completed"]
filter = { project = "$selected.project" }       # follows the selection in another view

[[view]]
id = "continuation"
type = "chain"
link_key = "continues"                           # follows session → previous session
label_key = "phase"                              # Concept → Design → Implementation → Re-Design → Implementation
scope = "$selected.issue"
```

The views lead to sessions. If the view config enables it (`create = true`, `drag = true`), they also
**create sessions in context**:

- *Tree node "+"*: a new session pre-seeded with the facts of the node's path (`project`, `branch`, `issue`).
- *Board column "+"*: a new session with `phase=<column>` plus the active filter facts.
  If `drag` is enabled, dragging a card to another column re-announces `phase` (with `source=User`).
- *Chain "+ next"*: a new session with `continues=<last session>`, a chosen phase, and the facts it inherits.
  A hand-off summary of the previous session is injected as context.

When no view is configured, the user sees the plain session list from the baseline (§6.1).

### 4.4 Documents

Plugins provide documents based on the current session's facts. Plugins do **not** render UI.
They return a document of a known **kind**, and the UI has one renderer per kind. This keeps plugins independent of
GPUI and keeps them reloadable.

| Document           | Kind            | Provided by                         | Triggered by facts      |
|--------------------|-----------------|-------------------------------------|-------------------------|
| P4 changelist      | `changeset`     | Perforce (VCS)                      | `changelist`            |
| Git worktree changes | `changeset`   | Git (VCS)                           | `worktree`              |
| Markdown view      | `markdown`      | core (files)                        | `doc` / any `.md` ref   |
| Code review        | `review`        | GitHub / GitLab / P4 Swarm (Forge)  | `review`, `branch`      |
| Design decisions   | `decision-list` | core (decision log)                 | `project`, `issue`      |
| Build status       | `build-status`  | TeamCity / Jenkins / forge CI       | `build`, `branch`       |

Further kinds: `table`, `log`, `tree`, `timeline`. A new kind needs a new renderer in the UI.
A new source of an existing kind only needs a plugin.

### 4.5 Workbench (multi-document UI)

The UI is a **multi-document workbench**, like an IDE. Sessions and documents are equal citizens: both open as tabs.

```
┌────────────┬─────────────────────────────────────────────────────┬──────────────┐
│ Left dock  │ Center: tabbed panes, splittable                    │ Right dock   │
│ navigator  │ [Chat s42] [Terminal s43] [Changes feat/x] [SPEC.md] │ context:     │
│ views      │ ┌─────────────────────────┬───────────────────────┐ │ documents of │
│ (session   │ │ chat / terminal         │ document              │ │ the active   │
│ list, tree,│ │                         │                       │ │ session's    │
│ board, …)  │ └─────────────────────────┴───────────────────────┘ │ facts        │
├────────────┴─────────────────────────────────────────────────────┴──────────────┤
│ Bottom dock: notifications, approvals, and agent-configured panels               │
└──────────────────────────────────────────────────────────────────────────────────┘
```

- **Items.** Everything that opens in a tab is an item with a stable URI:
  - sessions: `tk://session/<id>`
  - documents: `tk://doc/<kind>/<provider>/<ref>`

  Navigator views live in docks. The view config sets which dock each view goes in.
- **Center area.** Tabs sit in panes, and panes split horizontally and vertically. Tabs can be dragged between panes.
  Any mix is possible, for example a chat next to the worktree diff it is producing.
- **Preview tabs.** Opening a document from the context dock or a view uses a single preview tab. The next preview
  replaces it unless the tab was pinned.
- **Context dock.** It follows the active session and lists the documents its facts trigger (§4.4).
- **Live documents.** Open documents update when their source changes (plugin events, fact changes). There is no manual refresh.
- **Multiple windows.** A window can be scoped to a fact filter, for example `project = tratschkathl`. Its navigator and
  context dock then show only matching sessions.
- **Agents and layout.**
  - Agents can put an item in front of the user with `document_open(uri, placement?)` and `session_focus(id)`.
    The placement is `preview`, `pane:<direction>` or `window:new`.
  - Agents can open items, but they never close or rearrange the user's tabs and splits.
- **Persistence.** The layout (windows, panes, tabs, docks and sizes) is saved continuously and is part of the UI handoff
  (§3.4). It survives every reload and restart.
- **Built on GPUI primitives.** Zed's workspace and pane code is too tied to Zed to reuse as a library. We build a small
  workbench (window, dock, pane, tab, item) of our own.

---

## 5. Layers and plugin capabilities

```
┌ L4 UI (tk-ui, GPUI) ───────────────────────────────────────────────────────────────────────────┐
│ Baseline shell + settings · Navigator views · Chat view · Terminal view · Document renderers    │
│ Agent-configured commands                                                                      │
├ L3 Agent Handler (tk-agents) ──────────────────────────────────────────────────────────────────┤
│ ACP client · Terminal emulation · Session lifecycle · Plugins with the Agent capability (Claude) │
├ L2 Services (tk-services) ─────────────────────────────────────────────────────────────────────┤
│ Session registry · Fact store · View engine · Command registry · Document router · Decision log │
│ Automation · Resource manager · Event log · Config · MCP tool implementations                  │
│ Plugin host: Vcs · Forge · Build · WorkItems · DocumentProvider · FactDeriver · EventSource · …  │
├ L1 Core (tk-core) ─────────────────────────────────────────────────────────────────────────────┤
│ Bus · Layer supervisor · Build / stage / self-test · Versions · Request queue · Handoff · Shims  │
├ Helpers ───────────────────────────────────────────────────────────────────────────────────────┤
│ tk-launcher · tk-shim (per session) · tk-mcp-bridge (per session)                               │
└────────────────────────────────────────────────────────────────────────────────────────────────┘
  Capability traits (tk-plugin-api) ← Plugins: Claude · Git · Perforce · GitHub · GitLab · TeamCity · Jenkins · Jira · Asana · Trello · …
```

Where each part of a plugin runs:

- The `Agent` capability runs in the **Agent Handler**.
- All other capabilities run in **Services**.
- The UI renders only document **kinds**. It never runs plugin code.

A plugin implements **one or more** capabilities. GitHub, for example, is Forge + WorkItems + Build (Actions).

| Capability          | Responsibility                                                              |
|---------------------|-----------------------------------------------------------------------------|
| `Agent`             | Start, resume and cancel sessions. Mode: `Acp { command }` or `Terminal { command }` |
| `Vcs`               | Workspaces (git worktree / P4 client), branches/streams, pending changes, diff |
| `Forge`             | Review requests (PR / MR / Swarm review), comments, review state            |
| `Build`             | Build configs, runs, status, logs, trigger                                  |
| `WorkItems`         | Issues/tickets, states, transitions, comments                               |
| `DocumentProvider`  | Facts → documents of a known kind                                           |
| `FactDeriver`       | Derives facts from session context (cwd, branch name → issue key, …)        |
| `EventSource`       | Emits events for automation (webhook, polling, file watch)                  |
| `RefResolver`       | Resolves `Ref(kind, id)` into a title, URL and status                       |

### 5.1 Plugin roadmap

| Layer          | Plugin     | Capabilities                                | Status   |
|----------------|------------|---------------------------------------------|----------|
| Agents         | Claude     | Agent (ACP + Terminal)                      | **now**  |
| Repository     | Git        | Vcs, FactDeriver, DocumentProvider          | **now**  |
| Repository     | Perforce   | Vcs, FactDeriver, DocumentProvider          | next     |
| Repository     | GitHub     | Forge, WorkItems, Build, EventSource, RefResolver | next |
| Repository     | GitLab     | Forge, WorkItems, Build, EventSource, RefResolver | later |
| Build services | TeamCity   | Build, EventSource, DocumentProvider        | next     |
| Build services | Jenkins    | Build, EventSource, DocumentProvider        | later    |
| Work mgmt      | Jira       | WorkItems, RefResolver, EventSource         | next     |
| Work mgmt      | Asana      | WorkItems, RefResolver                      | later    |
| Work mgmt      | Trello     | WorkItems, RefResolver                      | later    |

"Later" plugins get a crate stub with a capability declaration early on. This keeps the traits honest:
if a trait cannot express Perforce or Trello, it is too Git- or Jira-shaped.

### 5.2 Plugin model

- **v1: in-process Rust crates** behind the stable `tk-plugin-api` traits, registered at compile time.
  Hot reload comes for free, because the layer that hosts the plugin reloads (§3): Services for most plugins,
  the Agent Handler for agent plugins. We do not load Rust dylibs at runtime,
  because Rust has no stable ABI.
- **v2: out-of-process plugins** over JSON-RPC on stdio, the same capability surface serialized.
  This allows plugins in any language and reloading a single plugin. We do this when the first third-party
  plugin needs it.
- Plugin config and secrets live in config / the OS keychain, never in facts or in the event log.

---

## 6. Agent surface and user surface

The app is **agent-centric**. The full tool registry is exposed to agents: as ACP tools to agents hosted over ACP,
and over MCP to terminal agents (§4.1). Both paths use the same registry and the same names. The user does **not** get a
mirror of these tools. The user gets a minimal baseline (§6.1) plus the commands that agents configure for them.
Tools marked *gated* need user approval; the policy is a basic setting.

| Group       | Tools                                                                                   |
|-------------|-----------------------------------------------------------------------------------------|
| Facts       | `fact_announce(key, value, note?)` · `fact_clear(key, value?)` · `fact_list(session?)` · `fact_query(filter)` |
| Sessions    | `session_list(filter)` · `session_create(agent, prompt, facts)` *gated* · `session_message(id, text)` · `session_summary(id)` |
| Views       | `view_config_get()` · `view_config_set(toml)` (validated, versioned)                     |
| UI commands | `ui_command_list()` · `ui_command_set(command)` · `ui_command_remove(id)` (validated, versioned) |
| Documents   | `document_list()` (for current facts) · `document_open(uri, placement?)`                 |
| Workbench   | `session_focus(id)` · `layout_get()` (read-only)                                         |
| Decisions   | `decision_log(title, context, decision, alternatives, rationale)` · `decision_list(filter)` |
| Automation  | `automation_list()` · `automation_upsert(rule)` *gated* · `automation_disable(id)`       |
| Resources   | `resource_list()` · `resource_acquire(name, reason)` · `resource_release(name)` (coarse, no polling; see §7.3) |
| Host        | `host_status(ticket?)` · `host_reload(layers?, reason)` · `host_rollback(layer)` *gated* |

### 6.1 User surface

**Fixed baseline.** This is built into the UI, always present, and not configurable by agents:

- The **workbench** (§4.5): windows, docks, tabbed and split panes, the context dock, and a plain session list in the left dock.
- Chat and terminal tabs, a message input, and a **New session** action (choose an agent and a working folder).
  This is the bootstrap: the user needs it to start the first agent.
- Permission and approval prompts: ACP permission requests and gated tools.
- Notifications: a session is waiting, a reload succeeded or was rolled back, a resource was granted.
- **Basic settings:**
  - Agents: the command and path for Claude Code and for the ACP adapter, and the default agent.
  - Workspaces: known project folders.
  - Plugin accounts: endpoints and credentials, stored in the OS keychain.
  - Approval policy: which tools are gated.
  - Limits: the maximum number of concurrent sessions, and the automation kill switch.
  - Appearance: theme and font.
- **Safe mode** (a launcher flag and a menu entry): start the last known good version of every layer, with automations off.

**Agent-configured.** Everything else exists only after an agent adds it:

- Views and their interactions (`create`, `drag`; see §4.3).
- Commands, added with `ui_command_set`. A command has a label, a placement and an action.
  - The placement is the command palette, a view node's context menu, a session toolbar or a document toolbar.
  - The action calls a tool with templated arguments, sends a prompt to a session, or starts a session from a template.
  - Commands are stored in config and versioned like views.

```toml
[[command]]
id = "reload-ui"
label = "Reload UI"
placement = ["palette"]
action = { tool = "host_reload", args = { layers = ["ui"], reason = "user request" } }

[[command]]
id = "mark-designed"
label = "Mark as designed"
placement = ["view:board/card", "session/toolbar"]
action = { tool = "fact_announce", args = { session = "$session.id", key = "phase", value = "designed" } }
```

When a user runs a command, the action runs with `source=User` and is logged like any other tool call.

---

## 7. Orchestrator services

### 7.1 Decision log

- Records decisions with context, alternatives and rationale, linked to a session and its facts.
- Agents write to it through `decision_log`. The orchestrator writes to it **automatically** whenever it acts on its own:
  an automation started a session, a resource was granted to A before B, a reload was rolled back, a fact was derived.
- It is shown as a `decision-list` document, filtered by the current facts ("Design Decisions" for this issue).

### 7.2 Automation

A rule has the shape `trigger → condition → action`. Rules are defined in config and can be edited through MCP.

```toml
[[automation]]
id = "build-failed-investigate"
trigger = { event = "build.failed" }                 # or { cron = "0 7 * * MON-FRI" } / { fact = "phase", value = "designed" }
condition = "event.branch != 'main'"
action = { start_session = { template = "investigate-build", facts = { branch = "$event.branch", build = "$event.build" } } }
# alternative: { message_session = { match = { branch = "$event.branch", phase = "implementing" }, text = "…" } }
limits = { max_concurrent = 2, cooldown = "30m" }
```

- Triggers: time (cron), plugin events (build failed, PR comment, issue moved), fact changes, file changes.
- Actions: start a session from a template, or send a message to an existing session that matches the facts.
- Guardrails: per-rule limits, a global cap on concurrent sessions, a kill switch, and a decision-log entry for every firing.

### 7.3 Resource management

Resource management is **coarse-grained**. It lets agents agree on a few simple, shared things. It does not schedule,
plan or micro-manage an agent's work. The typical question it answers: *several agents have a build ready. Who gets to
run the full version of the app right now?*

Keep tool use and token overhead close to zero:

- **There are only a few resources, and each is coarse.** A resource is a named token with a capacity, usually 1.
  Examples:
  - `app-full-run`: the one machine slot for running the full app build.
  - `test-pc-01`: an external test computer.
  - `local-gpu-heavy`: "this PC is under heavy GPU load".
  Resources are defined in config. Agents do not invent them as they work.
- **Two calls per use, no polling.**
  - `resource_acquire(name, reason)` returns `granted` or `queued` at once.
  - If the agent was queued, the orchestrator **tells the session** when the resource is granted, by injecting a short message.
    The agent never polls.
  - `resource_release(name)` gives the resource back.
- **No heartbeats.** A hold ends at the first of these:
  - an explicit release;
  - the session ends;
  - the session is idle longer than the resource's `max_idle`;
  - the resource's `max_hold` runs out, with a notice to the holder.
- **A simple FIFO queue.** There are no priorities and no counts.
- **The orchestrator uses resources itself, implicitly.** For example, `host_reload` from a worktree takes `app-full-run` for
  its duration, so the agent spends no tool calls on it.
- **Visibility.** Grants and timeouts appear as notifications and in the decision log. A view of holders and queues
  exists only if an agent configures one.

```toml
[[resource]]
name = "app-full-run"
capacity = 1
max_hold = "45m"
max_idle = "10m"
```

### 7.4 Visualizing project change — *exploratory*

This is the least clear part. The plan is to build the foundation first and decide on the visual later.

- **Foundation, needed anyway:** a unified event log that holds fact changes, session state changes, commits and changelists,
  builds, reviews and decisions, all with timestamps and fact context. Every visualization is a projection of this log.
  For ACP agents, the log also records every file read and write that goes through `fs/*`.
- **Candidate visuals** (to prototype one by one):
  1. A *live activity timeline* per tree node (project / branch / issue).
  2. An *area heatmap*: which files or modules active sessions are touching right now. This also **warns about overlap**
     between parallel sessions, which is the most useful candidate.
  3. A *correlation graph*: sessions ↔ branches ↔ issues ↔ reviews ↔ builds.
  4. A *replay* of how a feature moved through its continuation chain.

---

## 8. Workspace layout

```
crates/
  tk-launcher      bin   external bootstrapper; reboots and rolls back Core; safe mode
  tk-core          bin   L1: bus, layer supervisor, build/stage/self-test, versions, handoff, shim registry
  tk-services      bin   L2: orchestrator domain, plugin host, MCP tool implementations
  tk-agents        bin   L3: ACP client, terminal emulation, session lifecycle, agent plugins
  tk-ui            bin   L4: GPUI app: workbench, baseline + settings, views, renderers, configured commands
  tk-shim          bin   per-session PTY/stdio holder and stream recorder
  tk-mcp-bridge    bin   per-session stdio MCP server → Core
  tk-bus           lib   IPC transport, versioned interface definitions, handoff types, self-test harness
  tk-domain        lib   sessions, facts, views, commands, decisions, automation, resources, event log, storage, config
  tk-acp           lib   ACP client side
  tk-term          lib   terminal model (alacritty_terminal) + grid snapshot/diff protocol
  tk-plugin-api    lib   capability traits, document model, Ref types
plugins/
  tk-plugin-claude, tk-plugin-git, tk-plugin-perforce, tk-plugin-github, tk-plugin-gitlab,
  tk-plugin-teamcity, tk-plugin-jenkins, tk-plugin-jira, tk-plugin-asana, tk-plugin-trello
docs/
  SPEC.md
```

---

## 9. Milestones

| M  | Content                                                                                          | Done when |
|----|--------------------------------------------------------------------------------------------------|-----------|
| M0 | **Bootstrap.** Workspace; launcher, all four layer processes (Services and Agent Handler may start out thin), shim, bridge; baseline workbench (left dock with the session list, tabbed and split center, terminal tabs, New session); versioned builds; `host_reload` per layer with self-test and rollback; safe mode | Claude Code running in the app edits the app, reloads the UI and then Core, and its session survives both |
| M1 | Fact store, fact tools over the bridge, Git fact deriver, tree view, "+" in context               | Sessions are grouped by project/branch from facts |
| M2 | ACP chat session (Claude) with orchestrator tools as ACP tools and ACP `fs/*` / `terminal/*` client capabilities; markdown and Git-changes documents in tabs and the context dock | Chat and terminal sessions side by side, with documents shown by facts |
| M3 | Board and chain views, `view_config_set`, `ui_command_set`, decision log                          | An agent configures the board and its commands through MCP |
| M4 | Resources, automation (cron + fact + build events), first Build plugin, GitHub plugin            | A build failure starts an investigation session automatically |
| M5 | Perforce, Jira, event-log visualization prototype                                                | — |

---

## 10. Open questions

1. **GPUI source**: depend on a pinned Zed git commit, or on a crates.io release? It moves fast, so pin it and upgrade on purpose.
2. **Terminal state across an Agent Handler reload**: is replaying the shim's ring buffer plus a forced redraw good enough,
   or should the shim also store periodic VT snapshots, which give exact scrollback but make the shim less dumb?
3. **Gated tools**: which tools need user approval by default once sessions develop the app on their own?
   Is `ui_command_set` gated? It changes what the user sees.
4. **Fact trust**: should agent-announced facts and plugin-derived facts be weighted differently when they conflict?
   *M1 rule:* a derived fact never overrides a fact that an agent or the user set for the same key. A plugin only
   replaces or clears its own facts. Is that enough, or do we need explicit weights?
5. **Plugin v2 timing**: out-of-process JSON-RPC, or the WASM component model?
6. **Visualization**: which candidate in §7.4 should be prototyped first?
7. **Bus serialization**: JSON, which is easy to debug, or a binary format (postcard), which suits terminal grid diffs?
   Maybe JSON for control messages and binary for streams.
8. **Helper upgrades**: how do running sessions move to a new `tk-shim` / `tk-mcp-bridge` version? Only new sessions,
   or an opt-in "migrate session" that uses resume?
9. **ACP tool delivery**: which mechanism does the Claude ACP adapter support today for client-provided tools
   (MCP-over-ACP, extension methods, or only `mcpServers`)? *M2 status:* the tools reach chat agents through the
   `tk-mcp-bridge` declared in `session/new { mcpServers }`. The client serves `fs/*`. It does not offer `terminal/*`
   yet, so the agent runs commands itself.
10. **Global search**: deliberately left out for now and needs more thought. What would it search (sessions, transcripts,
    facts, documents, decisions, plugin sources), and how would it relate to agents?
