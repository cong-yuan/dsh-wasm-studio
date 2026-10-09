//! Bounded read-only projection of the native persisted Session Event ledger.
//! Do not return raw stream chunks, base64 attachments or entire tool schemas.
use crate::studio::Studio;
use anyhow::{ensure, Result};
use dsh_rs::types::{ContentBlock, Message, SessionEvent, SessionEventData, StreamChunk};
use serde_json::{json, Value};
use std::collections::HashMap;

const MAX_CONTENT: usize = 24_000;
fn clipped(value: &str) -> String {
    if value.chars().count() <= MAX_CONTENT {
        return value.into();
    }
    let text: String = value.chars().take(MAX_CONTENT).collect();
    format!("{text}\n[Content truncated for display]")
}
fn message_text(message: &Message) -> String {
    let mut text = String::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text: t } | ContentBlock::Reasoning { text: t } => {
                if text.len() < MAX_CONTENT {
                    text.push_str(t)
                }
            }
            ContentBlock::ToolResult { content, .. } => {
                for inner in content {
                    if let ContentBlock::Text { text: part } = inner {
                        if text.len() < MAX_CONTENT {
                            text.push_str(part)
                        }
                    }
                }
            }
            ContentBlock::Image { .. } => text.push_str("[image attachment]"),
            _ => {}
        }
    }
    clipped(&text)
}
fn event_row(event: &SessionEvent, turn: u64, step: u64) -> Option<Value> {
    use SessionEventData::*;
    let (role, kind, label, content, input, output, usage, call_id) = match &event.data {
        TurnStart { .. } => (
            "context",
            "turn/start",
            "Turn begins",
            String::new(),
            None,
            None,
            None,
            None,
        ),
        TurnEnd { reason, .. } => (
            "context",
            "turn/end",
            "Turn ends",
            format!("{reason:?}"),
            None,
            None,
            None,
            None,
        ),
        StepStart { .. } => (
            "model",
            "step/start",
            "Model step begins",
            String::new(),
            None,
            None,
            None,
            None,
        ),
        StepEnd { .. } => (
            "model",
            "step/end",
            "Model step ends",
            String::new(),
            None,
            None,
            None,
            None,
        ),
        UserMessage { message } => (
            "user",
            "user/message",
            "User",
            message_text(message),
            None,
            None,
            None,
            None,
        ),
        AssistantMessage {
            message,
            usage,
            interrupted: _,
            ..
        } => (
            "assistant",
            "assistant/message",
            "Assistant",
            message_text(message),
            None,
            None,
            usage
                .as_ref()
                .map(|u| serde_json::to_value(u).unwrap_or(Value::Null)),
            None,
        ),
        ToolCall {
            call_id,
            name,
            arguments,
            ..
        } => (
            "tool",
            "tool/call",
            name.as_str(),
            clipped(arguments),
            Some(clipped(arguments)),
            None,
            None,
            Some(call_id.clone()),
        ),
        ToolResult { message, .. } => {
            let id = message.content.iter().find_map(|block| match block {
                ContentBlock::ToolResult { tool_call_id, .. } => Some(tool_call_id.clone()),
                _ => None,
            });
            let result = message_text(message);
            (
                "tool",
                "tool/result",
                "Tool result",
                result.clone(),
                None,
                Some(result),
                None,
                id,
            )
        }
        Compaction { summary } => (
            "context",
            "session/compact",
            "Compaction",
            clipped(summary),
            None,
            None,
            None,
            None,
        ),
        RequestHeader { header } => (
            "system",
            "request/header",
            "Model request",
            header.system.as_deref().map(clipped).unwrap_or_default(),
            None,
            None,
            None,
            None,
        ),
        TodoWrite { todos } => (
            "context",
            "todo/write",
            "Todo state",
            format!("{} todo items", todos.len()),
            None,
            None,
            None,
            None,
        ),
        AuthorizedFolders { folders } => (
            "context",
            "session/authorized-folders",
            "Authorized folders",
            format!("{} folders", folders.len()),
            None,
            None,
            None,
            None,
        ),
        SessionEndSeed => (
            "context",
            "session/end-seed",
            "Session seed",
            String::new(),
            None,
            None,
            None,
            None,
        ),
        AssistantChunk { .. } => return None,
    };
    let mut row = json!({"seq":event.seq,"time":event.time,"turn":turn,"step":step,
        "role":role,"kind":kind,"label":label,"content":content});
    if let Some(value) = input {
        row["input"] = json!(value)
    }
    if let Some(value) = output {
        row["output"] = json!(value)
    }
    if let Some(value) = usage {
        row["usage"] = value
    }
    if let Some(value) = call_id {
        row["callId"] = json!(value)
    }
    Some(row)
}

pub(crate) fn trajectory_projection(
    events: &[SessionEvent],
    before: Option<u64>,
    limit: usize,
) -> Value {
    let mut rows: Vec<Value> = Vec::new();
    let mut turn = 0;
    let mut step = 0;
    let mut pending_steps: HashMap<(u64, u64), usize> = HashMap::new();
    let mut pending_calls: HashMap<(u64, u64, String), usize> = HashMap::new();
    let mut pending_turns: HashMap<u64, usize> = HashMap::new();
    let mut first_token: HashMap<(u64, u64), u64> = HashMap::new();
    let mut last_token: HashMap<(u64, u64), u64> = HashMap::new();
    for event in events {
        match &event.data {
            SessionEventData::TurnStart { turn: t } => {
                turn = *t;
                step = 0;
            }
            SessionEventData::TurnEnd { turn: t, .. } => {
                turn = *t;
            }
            SessionEventData::StepStart { turn: t, step: s }
            | SessionEventData::StepEnd { turn: t, step: s }
            | SessionEventData::AssistantMessage {
                turn: t, step: s, ..
            }
            | SessionEventData::ToolCall {
                turn: t, step: s, ..
            }
            | SessionEventData::ToolResult {
                turn: t, step: s, ..
            }
            | SessionEventData::AssistantChunk {
                turn: t, step: s, ..
            } => {
                turn = *t;
                step = *s;
            }
            _ => {}
        }
        // TTFT and decode duration are only recorded when real timestamped
        // model token events exist. No synthetic per-token timing estimates.
        if let SessionEventData::AssistantChunk {
            turn: t,
            step: st,
            chunk,
        } = &event.data
        {
            let has_token = match chunk {
                StreamChunk::TextDelta { text, .. } | StreamChunk::ReasoningDelta { text, .. } => {
                    !text.is_empty()
                }
                _ => false,
            };
            if has_token {
                let key = (*t, *st);
                if let Some(index) = pending_steps.get(&key).copied() {
                    if let std::collections::hash_map::Entry::Vacant(slot) = first_token.entry(key)
                    {
                        slot.insert(event.time);
                        rows[index]["ttftMs"] = json!(event
                            .time
                            .saturating_sub(rows[index]["time"].as_u64().unwrap_or(event.time)));
                    }
                    last_token.insert(key, event.time);
                }
            }
        }
        if let Some(mut row) = event_row(event, turn, step) {
            let idx = rows.len();
            match &event.data {
                SessionEventData::TurnStart { turn: t } => {
                    pending_turns.insert(*t, idx);
                }
                SessionEventData::TurnEnd { turn: t, .. } => {
                    if let Some(start) = pending_turns.remove(t) {
                        rows[start]["endAt"] = json!(event.time);
                        rows[start]["durationMs"] = json!(event
                            .time
                            .saturating_sub(rows[start]["time"].as_u64().unwrap_or(event.time)));
                    }
                }
                SessionEventData::StepStart { turn: t, step: s } => {
                    pending_steps.insert((*t, *s), idx);
                }
                SessionEventData::StepEnd { turn: t, step: s } => {
                    if let Some(start) = pending_steps.remove(&(*t, *s)) {
                        let key = (*t, *s);
                        if let (Some(first), Some(last)) =
                            (first_token.remove(&key), last_token.remove(&key))
                        {
                            rows[start]["decodeMs"] = json!(last.saturating_sub(first));
                        }
                        rows[start]["endAt"] = json!(event.time);
                        rows[start]["durationMs"] = json!(event
                            .time
                            .saturating_sub(rows[start]["time"].as_u64().unwrap_or(event.time)));
                    }
                }
                SessionEventData::ToolCall {
                    turn: t,
                    step: s,
                    call_id,
                    ..
                } => {
                    pending_calls.insert((*t, *s, call_id.clone()), idx);
                }
                SessionEventData::ToolResult {
                    turn: t,
                    step: s,
                    message,
                } => {
                    let id = message.content.iter().find_map(|block| match block {
                        ContentBlock::ToolResult { tool_call_id, .. } => Some(tool_call_id.clone()),
                        _ => None,
                    });
                    if let Some(id) = id {
                        if let Some(start) = pending_calls.remove(&(*t, *s, id)) {
                            rows[start]["endAt"] = json!(event.time);
                            rows[start]["durationMs"] = json!(event.time.saturating_sub(
                                rows[start]["time"].as_u64().unwrap_or(event.time)
                            ));
                        }
                    }
                }
                _ => {}
            }
            // Every row uses actual persisted times, not invented durations.
            row["status"] = match &event.data {
                SessionEventData::TurnEnd { .. } => json!("ended"),
                SessionEventData::ToolResult { message, .. } => {
                    let failed = message.content.iter().any(|block| {
                        matches!(
                            block,
                            ContentBlock::ToolResult {
                                is_error: Some(true),
                                ..
                            }
                        )
                    });
                    json!(if failed { "failed" } else { "succeeded" })
                }
                _ => Value::Null,
            };
            rows.push(row);
        }
    }
    let total = rows.len();
    let eligible = rows
        .iter()
        .take_while(|row| before.is_none_or(|b| row["seq"].as_u64().unwrap_or(0) < b))
        .count();
    let start = eligible.saturating_sub(limit.clamp(1, 500));
    let page = rows[start..eligible].to_vec();
    let next_before = if start > 0 {
        page.first().and_then(|row| row["seq"].as_u64())
    } else {
        None
    };
    json!({"ok":true,"records":page,"total":total,"hasMore":start>0,"nextBefore":next_before})
}

/// Latest durable TodoWrite snapshot, including an intentional empty list.
/// Historical UI reconstruction from tool call args cannot recover this event
/// when a dsh tool writes todos without an assistant ToolCall block.
pub(crate) fn session_todo_projection(events: &[SessionEvent]) -> Value {
    let snapshot = events.iter().rev().find_map(|event| match &event.data {
        SessionEventData::TodoWrite { todos } => Some((event.seq, todos)),
        _ => None,
    });
    match snapshot {
        Some((seq, todos)) => json!({"ok":true,"source":"session-event", "revision":seq,
            "todos":todos.iter().map(|item|json!({
                "content":item.content,
                "activeForm":item.content,
                "status":match item.status {
                    dsh_rs::types::TodoStatus::Pending=>"pending",
                    dsh_rs::types::TodoStatus::InProgress=>"in_progress",
                    dsh_rs::types::TodoStatus::Completed=>"completed",
                }
            })).collect::<Vec<Value>>() }),
        None => json!({"ok":true,"source":"no-event","revision":Value::Null,"todos":[]}),
    }
}

impl Studio {
    pub fn session_todos(&self, agent_id: &str) -> Result<Value> {
        ensure!(
            !agent_id.is_empty()
                && agent_id.len() <= 160
                && agent_id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "invalid todo session ID"
        );
        let events = match self.agent(agent_id) {
            Ok(agent) => agent.session().events(),
            Err(_) => self.stored_events(agent_id)?,
        };
        Ok(session_todo_projection(&events))
    }

    pub fn session_trajectory(
        &self,
        agent_id: &str,
        before: Option<u64>,
        limit: Option<usize>,
    ) -> Result<Value> {
        ensure!(
            !agent_id.is_empty()
                && agent_id.len() <= 160
                && agent_id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "invalid trajectory session ID"
        );
        let events = match self.agent(agent_id) {
            Ok(agent) => agent.session().events(),
            Err(_) => self.stored_events(agent_id)?,
        };
        Ok(trajectory_projection(&events, before, limit.unwrap_or(200)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_rs::types::{SessionEventData as Data, TurnEndReason};
    #[test]
    fn timeline_uses_actual_event_times_and_stable_tail_pagination() {
        let events = vec![
            SessionEvent::new(0, 1000, Data::TurnStart { turn: 1 }),
            SessionEvent::new(1, 1025, Data::StepStart { turn: 1, step: 1 }),
            SessionEvent::new(
                2,
                1035,
                Data::AssistantChunk {
                    turn: 1,
                    step: 1,
                    chunk: StreamChunk::TextDelta {
                        index: 0,
                        text: "Hello".into(),
                    },
                },
            ),
            SessionEvent::new(
                3,
                1045,
                Data::AssistantChunk {
                    turn: 1,
                    step: 1,
                    chunk: StreamChunk::TextDelta {
                        index: 0,
                        text: " world".into(),
                    },
                },
            ),
            SessionEvent::new(4, 1050, Data::StepEnd { turn: 1, step: 1 }),
            SessionEvent::new(
                3,
                1130,
                Data::TurnEnd {
                    turn: 1,
                    reason: TurnEndReason::Completed,
                },
            ),
        ];
        let full = trajectory_projection(&events, None, 100);
        assert_eq!(full["records"][0]["durationMs"], 130);
        assert_eq!(full["records"][1]["durationMs"], 25);
        assert_eq!(full["records"][1]["ttftMs"], 10);
        assert_eq!(full["records"][1]["decodeMs"], 10);
        assert_eq!(full["records"][0]["role"], "context");
        let tail = trajectory_projection(&events, None, 2);
        assert_eq!(tail["records"].as_array().unwrap().len(), 2);
        assert_eq!(tail["nextBefore"], 4);
        let prior = trajectory_projection(&events, Some(4), 2);
        assert_eq!(prior["records"][0]["seq"], 0);
        assert_eq!(prior["records"][1]["seq"], 1);
        assert_eq!(prior["hasMore"], false);
    }
    #[test]
    fn native_todo_snapshot_is_authoritative_even_when_cleared() {
        use dsh_rs::types::{TodoItem, TodoStatus};
        let events = vec![
            SessionEvent::new(
                1,
                100,
                Data::TodoWrite {
                    todos: vec![
                        TodoItem {
                            content: "Read source".into(),
                            status: TodoStatus::InProgress,
                        },
                        TodoItem {
                            content: "Write tests".into(),
                            status: TodoStatus::Pending,
                        },
                    ],
                },
            ),
            SessionEvent::new(
                2,
                150,
                Data::TodoWrite {
                    todos: vec![TodoItem {
                        content: "Read source".into(),
                        status: TodoStatus::Completed,
                    }],
                },
            ),
        ];
        let projected = session_todo_projection(&events);
        assert_eq!(projected["source"], "session-event");
        assert_eq!(projected["revision"], 2);
        assert_eq!(projected["todos"][0]["status"], "completed");
        assert_eq!(projected["todos"].as_array().unwrap().len(), 1);
        let cleared = vec![
            events[0].clone(),
            SessionEvent::new(3, 200, Data::TodoWrite { todos: vec![] }),
        ];
        assert!(session_todo_projection(&cleared)["todos"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(session_todo_projection(&cleared)["source"], "session-event");
        assert_eq!(session_todo_projection(&[])["source"], "no-event");
    }

    #[test]
    fn no_duration_is_fabricated_for_open_steps() {
        let events = vec![SessionEvent::new(
            7,
            9000,
            Data::StepStart { turn: 2, step: 3 },
        )];
        let row = trajectory_projection(&events, None, 200);
        assert!(row["records"][0].get("durationMs").is_none());
        assert!(row["records"][0].get("endAt").is_none());
    }
}
