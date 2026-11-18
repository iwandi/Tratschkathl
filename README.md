# Tratschkathl

A desktop orchestrator for LLM agent sessions, built in Rust with GPUI. It develops itself:
the agents that work on Tratschkathl run inside Tratschkathl, and the app reloads itself
layer by layer while their sessions keep running. See [docs/SPEC.md](docs/SPEC.md).

## Status: M3 (views, commands, decisions)

M0 (bootstrap), M1 (facts and the tree view), M2 (chat sessions and documents) and M3 are done.

- `tk-launcher`: stages builds into versioned folders, starts Core, reboots it and rolls it back.
- `tk-core` (L1): bus hub, layer supervisor, `host_reload` / `host_rollback`, handoff store.
- `tk-services` (L2): session registry, fact store, view engine, tool registry, and the plugin host.
- `tk-agents` (L3): session lifecycle, terminal emulation (alacritty_terminal), and the ACP client for chat sessions.
- `tk-ui` (L4): the GPUI workbench. It has the configured views and a session list on the left;
  tabbed and split panes with terminal, chat and document tabs; and a context dock with the
  session's facts and documents.
- `tk-plugin-api`: capability traits. `plugins/tk-plugin-git` derives `project`, `repo`, `branch`
  and `worktree` facts from each session's folder.
- `tk-shim`: one per session. It owns the PTY (terminal agents) or the stdio pipes and full log
  (ACP agents), so every layer can restart without killing the agent.
- `tk-mcp-bridge`: the stdio MCP server that Claude Code starts. It holds tool calls while layers reload.
- `tk-ctl`: a small debugging client for the bus.

## Run

```powershell
cargo build
target\debug\tk-launcher.exe
```

The launcher copies the fresh binaries to `%LOCALAPPDATA%\Tratschkathl\versions\` and restarts
itself from there, so `target\` stays unlocked for the next build. Set `TK_HOME` to use a
different data folder. Logs are written to `%LOCALAPPDATA%\Tratschkathl\logs\`.

- `--safe-mode` starts the last known good version of every layer.
- `--headless` runs without the UI.

Agents are configured in `%LOCALAPPDATA%\Tratschkathl\settings.toml`. The defaults are Claude Code
(`claude`, which must be on `PATH`) and PowerShell. Claude Code sessions automatically get the
`tratschkathl` MCP server with these tools:

- facts: `fact_announce`, `fact_clear`, `fact_list`, `fact_query`
- documents: `document_list`, `document_open`
- decisions: `decision_log`, `decision_list`
- workbench: `view_config_get`, `view_config_set`, `ui_command_list`, `ui_command_set`, `ui_command_remove`
- sessions: `session_list`, `session_focus`
- host: `host_status`, `host_reload`, `host_rollback`

## Chat sessions (ACP)

"Claude (chat)" runs the Claude ACP adapter (`npx -y @agentclientprotocol/claude-agent-acp`). It needs
Node.js and a Claude login. The chat shows streamed answers as Markdown, thoughts, tool calls with
diffs, the agent's plan, and permission requests with buttons. Tratschkathl serves the agent's
file reads and writes, and the orchestrator tools reach the agent through the MCP bridge.

`tools/mock_acp_agent.py` is a scripted ACP agent for testing without an LLM. The comment at the
top of the file shows its `settings.toml` entry.

## Documents

The context dock lists documents for the session's facts:

- the worktree's uncommitted changes (`changeset`, from the Git plugin)
- its Markdown files (root and `docs/`, plus any `doc` facts)

Open documents refresh by themselves while they are visible.

## Views, commands and decisions

Agents shape the workbench, and the user starts with a minimal surface (spec §6.1). Two files in the
app's data folder hold this. Both are versioned, so an agent's change can be rolled back:

- **`views.toml`**, set with `view_config_set`, defines the views:
  - `tree`: the left dock, grouped by facts
  - `board`: a kanban tab with columns by a fact; drag cards to change the fact, or press "+" in a column
  - `chain`: the continuation bar on top, with sessions linked by a `continues` fact and "+ next"

  A filter or scope can say `$selected.<key>` to follow the selected session.
- **`commands.toml`**, set with `ui_command_set`, defines commands for the command palette (Ctrl+Shift+P) and
  for the context dock. A command calls a tool, sends a prompt, or starts a session.

Decisions are project content. They live in `<project>/.tk/decisions/`, one Markdown file each, and are
committed with the code. Tratschkathl's own automatic decisions (reloads, rollbacks) show as "Host decisions".

## Facts and views

Every session carries facts, such as `project`, `branch`, `issue` and `phase`. Agents announce them,
the Git plugin derives them, and "+" in a view seeds them. A derived fact never overrides one
that an agent or the user set.

Views are defined in `%LOCALAPPDATA%\Tratschkathl\views.toml`, and edits show up live. The
default is a tree grouped by `project`, then `branch`, then `issue`. "+" on a node starts a
session in that context: it uses the node's facts and the folder of the newest session under the node.

## Developing inside the app

1. Start a Claude Code session on this checkout from the left dock.
2. Ask it to change something, then to call `host_reload`.
3. Core rebuilds the code (`cargo build`) and self-tests every changed binary. It then swaps the changed
   layers bottom-up and rolls back any layer that doesn't come up. The session keeps running.

Debugging:

```powershell
$ctl = (Get-ChildItem $env:LOCALAPPDATA\Tratschkathl\versions\helpers | Sort-Object LastWriteTime | Select-Object -Last 1).FullName + "\tk-ctl.exe"
& $ctl tool host_status
& $ctl watch host.
```
