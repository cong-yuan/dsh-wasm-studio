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
    pub shared_notes: String,
}

impl Default for RuntimeControls {
    fn default() -> Self {
        Self {
            thinking_level: "off".into(),
            permission_mode: "operate".into(),
            memory_enabled: false,
            memory_notes: String::new(),
            shared_notes: String::new(),
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

// A pending grant is bound to one precise Agent/tool/call/arguments tuple.
// Nothing is persisted; a process restart or dropped tool future denies it.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingApproval {
    pub id: String,
    pub agent_id: String,
    pub call_id: String,
    pub tool_name: String,
    pub arguments: serde_json::Value,
}
struct ApprovalEntry {
    request: PendingApproval,
    answer: tokio::sync::oneshot::Sender<bool>,
}
fn approval_queue() -> &'static Mutex<HashMap<String, ApprovalEntry>> {
    static QUEUE: OnceLock<Mutex<HashMap<String, ApprovalEntry>>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(HashMap::new()))
}
static APPROVAL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn pending_approvals() -> Vec<PendingApproval> {
    let mut rows = approval_queue()
        .lock()
        .unwrap()
        .values()
        .map(|entry| entry.request.clone())
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}

pub fn decide_approval(agent_id: &str, id: &str, approved: bool) -> Result<(), &'static str> {
    let mut queue = approval_queue().lock().unwrap();
    let entry = queue.get(id).ok_or("approval expired or already decided")?;
    if entry.request.agent_id != agent_id {
        return Err("approval Agent mismatch");
    }
    let entry = queue.remove(id).expect("checked pending entry");
    entry
        .answer
        .send(approved)
        .map_err(|_| "approval call no longer waiting")
}

struct PendingGuard(String);
impl Drop for PendingGuard {
    fn drop(&mut self) {
        approval_queue().lock().unwrap().remove(&self.0);
    }
}

pub async fn request_approval(
    agent_id: &str,
    call_id: &str,
    tool_name: &str,
    arguments: &serde_json::Value,
    signal: &crate::types::CancelToken,
) -> Result<(), &'static str> {
    if signal.is_cancelled() {
        return Err("Agent turn already cancelled");
    }
    if tool_denial(agent_id, tool_name).map(|(code, _)| code) != Some("APPROVAL") {
        return Err("Agent no longer requires approval");
    }
    let id = format!(
        "approval-{}",
        APPROVAL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let (sender, receiver) = tokio::sync::oneshot::channel();
    {
        let mut queue = approval_queue().lock().unwrap();
        if queue.len() >= 48 {
            return Err("too many pending approvals");
        }
        queue.insert(
            id.clone(),
            ApprovalEntry {
                request: PendingApproval {
                    id: id.clone(),
                    agent_id: agent_id.into(),
                    call_id: call_id.into(),
                    tool_name: tool_name.into(),
                    arguments: arguments.clone(),
                },
                answer: sender,
            },
        );
    }
    let _guard = PendingGuard(id);
    let granted = tokio::select! {
        _ = signal.cancelled() => return Err("Agent turn cancelled"),
        outcome = tokio::time::timeout(std::time::Duration::from_secs(90), receiver) => {
            match outcome {
                Ok(Ok(answer)) => answer,
                Ok(Err(_)) => return Err("approval channel closed"),
                Err(_) => return Err("approval timed out"),
            }
        }
    };
    if !granted {
        return Err("approval denied by user");
    }
    if signal.is_cancelled() {
        return Err("Agent turn cancelled");
    }
    if tool_denial(agent_id, tool_name).map(|(code, _)| code) != Some("APPROVAL") {
        return Err("Agent permission mode changed during approval");
    }
    Ok(())
}

/// Build the actual per-request system context at the runtime boundary.
/// Disabled Memory always excludes private and shared notes.
pub fn system_with_memory(agent_id: &str, system: Option<String>) -> Option<String> {
    let controls = get(agent_id);
    if !controls.memory_enabled {
        return system;
    }
    let mut entries = Vec::new();
    if !controls.memory_notes.is_empty() {
        entries.push(controls.memory_notes.as_str());
    }
    if !controls.shared_notes.is_empty() {
        entries.push(controls.shared_notes.as_str());
    }
    if entries.is_empty() {
        return system;
    }
    let prior = system.unwrap_or_default();
    Some(format!(
        "{prior}\n\n[Approved Agent memory]\n{}",
        entries.join("\n")
    ))
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
    fn system_memory_respects_agent_opt_in_and_deletion() {
        let agent = "memory-runtime-gate-test";
        set(
            agent,
            RuntimeControls {
                memory_enabled: false,
                memory_notes: "Private: chocolate".into(),
                shared_notes: "Shared: Chinese".into(),
                ..Default::default()
            },
        );
        assert_eq!(
            system_with_memory(agent, Some("base".into())).as_deref(),
            Some("base")
        );
        set(
            agent,
            RuntimeControls {
                memory_enabled: true,
                memory_notes: "Private: chocolate".into(),
                shared_notes: "Shared: Chinese".into(),
                ..Default::default()
            },
        );
        let actual = system_with_memory(agent, Some("base".into())).unwrap();
        assert!(actual.contains("Private: chocolate"));
        assert!(actual.contains("Shared: Chinese"));
        set(
            agent,
            RuntimeControls {
                memory_enabled: true,
                shared_notes: "".into(),
                ..Default::default()
            },
        );
        assert!(!system_with_memory(agent, Some("base".into()))
            .unwrap()
            .contains("Chinese"));
    }

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
    #[tokio::test]
    async fn one_time_approval_never_grants_another_call_and_cancels_cleanly() {
        let agent = "approval-runtime-test";
        set(
            agent,
            RuntimeControls {
                permission_mode: "ask".into(),
                ..Default::default()
            },
        );
        let signal = crate::types::CancelToken::new();
        let signal_task = signal.clone();
        let handle = tokio::spawn(async move {
            request_approval(
                agent,
                "call-123",
                "write_file",
                &serde_json::json!({"path":"/tmp/test"}),
                &signal_task,
            )
            .await
        });
        tokio::task::yield_now().await;
        let pending = pending_approvals()
            .into_iter()
            .find(|p| p.call_id == "call-123")
            .unwrap();
        assert_eq!(pending.arguments["path"], "/tmp/test");
        assert!(decide_approval("other-agent", &pending.id, true).is_err());
        assert!(decide_approval(agent, &pending.id, true).is_ok());
        assert!(decide_approval(agent, &pending.id, true).is_err());
        assert!(handle.await.unwrap().is_ok());
        assert!(!pending_approvals().iter().any(|p| p.id == pending.id));
        assert_eq!(tool_denial(agent, "write_file").unwrap().0, "APPROVAL");

        let cancel_token = crate::types::CancelToken::new();
        let token = cancel_token.clone();
        let pending_call = tokio::spawn(async move {
            request_approval(
                agent,
                "call-cancel",
                "bash",
                &serde_json::json!({"command":"touch file"}),
                &token,
            )
            .await
        });
        tokio::task::yield_now().await;
        let pending_id = pending_approvals()
            .into_iter()
            .find(|p| p.call_id == "call-cancel")
            .unwrap()
            .id;
        cancel_token.cancel();
        assert!(pending_call.await.unwrap().is_err());
        assert!(decide_approval(agent, &pending_id, true).is_err());
    }

    #[tokio::test]
    async fn denied_approval_and_policy_change_never_execute() {
        let agent = "approval-policy-test";
        set(
            agent,
            RuntimeControls {
                permission_mode: "ask".into(),
                ..Default::default()
            },
        );
        let handle = tokio::spawn(async move {
            request_approval(
                agent,
                "call-denied",
                "bash",
                &serde_json::json!({"cmd":"rm"}),
                &crate::types::CancelToken::new(),
            )
            .await
        });
        tokio::task::yield_now().await;
        let row = pending_approvals()
            .into_iter()
            .find(|p| p.call_id == "call-denied")
            .unwrap();
        decide_approval(agent, &row.id, false).unwrap();
        assert!(handle.await.unwrap().is_err());

        let policy = tokio::spawn(async move {
            request_approval(
                agent,
                "call-mode-change",
                "bash",
                &serde_json::json!({"cmd":"write"}),
                &crate::types::CancelToken::new(),
            )
            .await
        });
        tokio::task::yield_now().await;
        let row = pending_approvals()
            .into_iter()
            .find(|p| p.call_id == "call-mode-change")
            .unwrap();
        set(
            agent,
            RuntimeControls {
                permission_mode: "read_only".into(),
                ..Default::default()
            },
        );
        decide_approval(agent, &row.id, true).unwrap();
        assert!(policy.await.unwrap().is_err());
    }
    #[test]
    fn reasoning_rejects_plain_chat_models() {
        assert!(!supports_reasoning("gpt-4o-mini"));
        assert!(supports_reasoning("gpt-5"));
    }
}
