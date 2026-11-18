"""Mock ACP agent for testing Tratschkathl's chat sessions without a real LLM.

Speaks ACP (JSON-RPC 2.0, newline-delimited) on stdin/stdout. On every prompt it:
streams an answer, reports a tool call, reads README.md through the client (fs/read_text_file),
asks for permission (session/request_permission) and finishes the turn.

Configure it as an agent in settings.toml:
    [[agents]]
    id = "mock"
    name = "Mock ACP"
    command = "python"
    args = ["<path>/tools/mock_acp_agent.py"]
    kind = "acp"
"""
import json
import os
import sys
import time

state = {"cwd": ".", "prompt_id": None, "next": 1000, "waiting": {}}


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def update(update_kind, **fields):
    send({"jsonrpc": "2.0", "method": "session/update",
          "params": {"sessionId": "mock-1", "update": {"sessionUpdate": update_kind, **fields}}})


def ask(method, params, then):
    rid = state["next"]
    state["next"] += 1
    state["waiting"][rid] = then
    send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})


def finish(reason="end_turn"):
    send({"jsonrpc": "2.0", "id": state["prompt_id"], "result": {"stopReason": reason}})
    state["prompt_id"] = None


def after_permission(msg):
    outcome = msg.get("result", {}).get("outcome", {})
    choice = outcome.get("optionId", outcome.get("outcome"))
    update("tool_call_update", toolCallId="t2", status="completed" if choice == "allow" else "failed")
    update("agent_message_chunk", content={"type": "text", "text": f"\n\nPermission answer: **{choice}**. Done."})
    finish()


def after_read(msg):
    if "error" in msg:
        first = "(could not read: " + msg["error"].get("message", "") + ")"
    else:
        first = msg["result"]["content"].splitlines()[0] if msg["result"]["content"] else ""
    update("tool_call_update", toolCallId="t1", status="completed",
           content=[{"type": "content", "content": {"type": "text", "text": first}}])
    update("plan", entries=[{"content": "Read README", "priority": "medium", "status": "completed"},
                            {"content": "Ask for permission", "priority": "medium", "status": "in_progress"}])
    update("tool_call", toolCallId="t2", title="Write notes.md", kind="edit", status="pending",
           content=[{"type": "diff", "path": "notes.md", "oldText": None, "newText": "# Notes\nhello"}])
    ask("session/request_permission",
        {"sessionId": "mock-1", "toolCall": {"toolCallId": "t2", "title": "Write notes.md"},
         "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                     {"optionId": "deny", "name": "Deny", "kind": "reject_once"}]},
        after_permission)


def on_prompt(msg):
    state["prompt_id"] = msg["id"]
    text = " ".join(c.get("text", "") for c in msg["params"]["prompt"])
    update("agent_thought_chunk", content={"type": "text", "text": "Thinking about the request…"})
    for word in f"You said: *{text}*. Let me look at the README.".split(" "):
        update("agent_message_chunk", content={"type": "text", "text": word + " "})
        time.sleep(0.02)
    update("tool_call", toolCallId="t1", title="Read README.md", kind="read", status="in_progress")
    ask("fs/read_text_file", {"sessionId": "mock-1", "path": os.path.join(state["cwd"], "README.md"), "limit": 5},
        after_read)


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": msg["id"],
              "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": False}, "authMethods": []}})
    elif method == "session/new":
        state["cwd"] = msg["params"]["cwd"]
        servers = [s["name"] for s in msg["params"].get("mcpServers", [])]
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"sessionId": "mock-1"}})
        update("agent_message_chunk", content={"type": "text", "text": f"Mock agent ready in `{state['cwd']}`. MCP servers: {servers}"})
    elif method == "session/prompt":
        on_prompt(msg)
    elif method == "session/cancel":
        if state["prompt_id"] is not None:
            finish("cancelled")
    elif method is None and "id" in msg:
        then = state["waiting"].pop(msg["id"], None)
        if then:
            then(msg)
    elif "id" in msg:
        send({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": f"unknown method {method}"}})
