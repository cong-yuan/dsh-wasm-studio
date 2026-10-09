//! Live, per-agent control snapshots consumed at the actual model and tool
//! execution points. The Studio owns durable storage and calls `set` at boot
//! and after successful revisioned updates. No UI-only policy is consulted.
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeControls {
    pub thinking_level: String,
    pub permission_mode: String,
    pub memory_enabled: bool,
    pub memory_notes: String,
}

impl Default for RuntimeControls {
    fn default() -> Self {
        Self {
            thinking_level: "off".into(),
            permission_mode: "operate".into(),
            memory_enabled: false,
            memory_notes: String::new(),
        }
    }
}

fn registry() -> &'static Mutex<HashMap<String, RuntimeControls>> {
    static INSTANCE: OnceLock<Mutex<HashMap<String, RuntimeControls>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn set(agent_id: &str, controls: RuntimeControls) {
    registry()
        .lock()
        .unwrap()
        .insert(agent_id.to_owned(), controls);
}

pub fn get(agent_id: &str) -> RuntimeControls {
    registry()
        .lock()
        .unwrap()
        .get(agent_id)
        .cloned()
        .unwrap_or_default()
}

/// Only known read-only builtins are allowed without a write-capable mode.
/// Unknown/plugin commands fail closed because their effects are not proven.
pub fn tool_denial(agent_id: &str, tool_name: &str) -> Option<(&'static str, &'static str)> {
    let mode = get(agent_id).permission_mode;
    if mode == "operate" || mode == "auto" {
        return None;
    }
    if matches!(tool_name, "read_file" | "glob" | "grep") {
        return None;
    }
    match mode.as_str() {
        "ask" => Some((
            "APPROVAL",
            "tool call requires explicit user approval (no approval granted)",
        )),
        _ => Some((
            "DENIED",
            "read_only mode prohibits write or unknown tool execution",
        )),
    }
}

/// Reasoning is an explicit opt-in for OpenAI-compatible models that actually
/// accept `reasoning_effort`. A generic chat model must never be sent this key.
pub fn supports_reasoning(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
        || m.starts_with("gpt-5")
        || m.starts_with("gpt-6")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn modes_fail_closed_for_unknown_tool() {
        set(
            "runtime-policy-test",
            RuntimeControls {
                permission_mode: "ask".into(),
                ..Default::default()
            },
        );
        assert_eq!(
            tool_denial("runtime-policy-test", "bash").unwrap().0,
            "APPROVAL"
        );
        assert_eq!(tool_denial("runtime-policy-test", "read_file"), None);
        set(
            "runtime-policy-test",
            RuntimeControls {
                permission_mode: "read_only".into(),
                ..Default::default()
            },
        );
        assert_eq!(
            tool_denial("runtime-policy-test", "plugin_write")
                .unwrap()
                .0,
            "DENIED"
        );
    }
    #[test]
    fn reasoning_rejects_plain_chat_models() {
        assert!(!supports_reasoning("gpt-4o-mini"));
        assert!(supports_reasoning("gpt-5"));
    }
}
