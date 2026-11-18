//! Agent-configured UI commands (spec §6.1). The user surface is minimal; everything else
//! exists because an agent added it with `ui_command_set`. Stored in `<home>/commands.toml`
//! (versioned like views).
//!
//! ```toml
//! [[command]]
//! id = "mark-designed"
//! label = "Mark as designed"
//! placement = ["palette", "session"]
//! action = { tool = "fact_announce", args = { session = "$session.id", key = "phase", value = "designed" } }
//! ```
//!
//! Actions: `{ tool, args }` calls a tool, `{ prompt = "…" }` sends text to the session,
//! `{ start_session = { agent, facts } }` starts a session next to it. Templates in strings:
//! `$session.id`, `$session.cwd`, `$fact.<key>` (the selected session's facts).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::versioned::VersionedFile;

const DEFAULT: &str = "# UI commands (spec §6.1), added by agents with ui_command_set.\n";

pub const FILE: VersionedFile = VersionedFile { name: "commands", default_text: DEFAULT };

pub const PLACEMENTS: &[&str] = &["palette", "session"];

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UiCommand {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub placement: Vec<String>,
    pub action: Value,
}

#[derive(Serialize, Deserialize, Default)]
struct CommandFile {
    #[serde(default, rename = "command")]
    commands: Vec<UiCommand>,
}

pub fn parse(text: &str, tools: &[String]) -> Result<Vec<UiCommand>, String> {
    let file: CommandFile = toml::from_str(text).map_err(|e| format!("commands.toml: {e}"))?;
    let mut seen = std::collections::HashSet::new();
    for c in &file.commands {
        validate(c, tools)?;
        if !seen.insert(c.id.clone()) {
            return Err(format!("duplicate command id '{}'", c.id));
        }
    }
    Ok(file.commands)
}

pub fn validate(c: &UiCommand, tools: &[String]) -> Result<(), String> {
    if c.id.is_empty() || c.label.is_empty() {
        return Err("a command needs id and label".into());
    }
    for p in &c.placement {
        if !PLACEMENTS.contains(&p.as_str()) {
            return Err(format!("command '{}': unknown placement '{p}' ({PLACEMENTS:?})", c.id));
        }
    }
    let a = c.action.as_object().ok_or_else(|| format!("command '{}': action must be a table", c.id))?;
    let kinds = ["tool", "prompt", "start_session"].iter().filter(|k| a.contains_key(**k)).count();
    if kinds != 1 {
        return Err(format!("command '{}': action needs exactly one of tool, prompt, start_session", c.id));
    }
    if let Some(t) = a.get("tool").and_then(Value::as_str) {
        if !tools.iter().any(|x| x == t) {
            return Err(format!("command '{}': unknown tool '{t}'", c.id));
        }
    }
    if let Some(s) = a.get("start_session") {
        if s.get("agent").and_then(Value::as_str).is_none() {
            return Err(format!("command '{}': start_session needs an agent", c.id));
        }
    }
    Ok(())
}

pub fn render(commands: &[UiCommand]) -> String {
    let body = toml::to_string(&CommandFile { commands: commands.to_vec() }).unwrap_or_default();
    format!("{DEFAULT}\n{body}")
}

/// Replace `$session.id`, `$session.cwd` and `$fact.<key>` in every string of `v`.
pub fn fill(v: &Value, session: &str, cwd: &str, facts: &BTreeMap<String, String>) -> Value {
    match v {
        Value::String(s) => {
            let mut out = s.replace("$session.id", session).replace("$session.cwd", cwd);
            while let Some(i) = out.find("$fact.") {
                let rest = &out[i + 6..];
                let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')).unwrap_or(rest.len());
                let key = rest[..end].trim_end_matches('.').to_string();
                let value = facts.get(&key).cloned().unwrap_or_default();
                out.replace_range(i..i + 6 + key.len(), &value);
            }
            Value::String(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| fill(x, session, cwd, facts)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), fill(x, session, cwd, facts))).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_validate_fill() {
        let tools = vec!["fact_announce".to_string()];
        let ok = "[[command]]\nid='a'\nlabel='A'\nplacement=['palette']\naction={tool='fact_announce', args={session='$session.id', key='phase', value='designed'}}\n";
        let cmds = parse(ok, &tools).unwrap();
        assert_eq!(cmds[0].action["args"]["key"], "phase");
        assert!(parse("[[command]]\nid='a'\nlabel='A'\naction={tool='nope'}\n", &tools).is_err());
        assert!(parse("[[command]]\nid='a'\nlabel='A'\naction={prompt='x', tool='fact_announce'}\n", &tools).is_err());
        assert!(parse("[[command]]\nid='a'\nlabel='A'\nplacement=['menu']\naction={prompt='x'}\n", &tools).is_err());
        let facts: BTreeMap<String, String> = [("issue".to_string(), "TK-1".to_string())].into();
        let filled = fill(&json!({"text": "Work on $fact.issue in $session.cwd ($session.id)."}), "s1", "D:/x", &facts);
        assert_eq!(filled["text"], "Work on TK-1 in D:/x (s1).");
        // Round trip through the writer.
        assert_eq!(parse(&render(&cmds), &tools).unwrap().len(), 1);
    }
}
