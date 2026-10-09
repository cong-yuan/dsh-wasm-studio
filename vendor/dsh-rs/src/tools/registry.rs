//! The tool registry and guarded execution pipeline — the Rust analogue of
//! the reference harness's `packages/core/tools`.
//!
//! A [`ToolDefinition`] pairs a model-facing [`ToolSchema`] with an `execute`
//! body. [`ToolRegistry::execute`] runs one accepted call through the guarded
//! pipeline: `tools/pre-execute` (allow/deny decision waterfall) → monotonic
//! guards → `tools/execute` (around-dispatch wrapper waterfall) → the tool
//! body → `tools/post-execute` (result waterfall).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cordis::{plugin, Context, Plugin};
use serde_json::{json, Value};

use crate::llm::ToolSchema;
use crate::types::{ToolCallArgs, ToolExecutionResult, ToolRunContext};
use cordis::plugin::BoxFuture;

/// The `tools` service key.
pub const TOOLS_SERVICE: &str = "tools";

/// One registered tool: schema plus the execution function.
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema object for the arguments.
    pub parameters: Value,
    pub execute:
        Arc<dyn Fn(ToolCallArgs, ToolRunContext) -> BoxFuture<ToolExecutionResult> + Send + Sync>,
    pub is_concurrency_safe: bool,
}

impl ToolDefinition {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        execute: impl Fn(ToolCallArgs, ToolRunContext) -> BoxFuture<ToolExecutionResult>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        ToolDefinition {
            name: name.into(),
            description: description.into(),
            parameters,
            execute: Arc::new(execute),
            is_concurrency_safe: false,
        }
    }

    pub fn concurrency_safe(mut self) -> Self {
        self.is_concurrency_safe = true;
        self
    }

    pub fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }
}

/// A monotonic execution guard: returning `Some(reason)` denies the call;
/// `None` leaves it unchanged. Guards have no allow result, so listener
/// ordering cannot turn a denial back into permission.
pub type ToolGuard = Arc<dyn Fn(&ToolCallArgs) -> Option<String> + Send + Sync>;

struct ToolRegistryInner {
    tools: Mutex<HashMap<String, Arc<ToolDefinition>>>,
    guards: Mutex<Vec<ToolGuard>>,
    ctx: Context,
}

/// Scoped tool registry (`ctx.tools`). Cheap-clone service handle.
#[derive(Clone)]
pub struct ToolRegistry {
    inner: Arc<ToolRegistryInner>,
}

impl ToolRegistry {
    pub fn new(ctx: Context) -> Self {
        ToolRegistry {
            inner: Arc::new(ToolRegistryInner {
                tools: Mutex::new(HashMap::new()),
                guards: Mutex::new(Vec::new()),
                ctx,
            }),
        }
    }

    /// Register one tool; a duplicate name is replaced (last wins).
    pub fn register(&self, tool: Arc<ToolDefinition>) {
        self.inner
            .tools
            .lock()
            .unwrap()
            .insert(tool.name.clone(), tool);
    }

    pub fn unregister(&self, name: &str) {
        self.inner.tools.lock().unwrap().remove(name);
    }

    /// Add a monotonic guard evaluated before every dispatch.
    pub fn add_guard(&self, guard: ToolGuard) {
        self.inner.guards.lock().unwrap().push(guard);
    }

    /// The model-facing schemas of every registered tool.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.inner
            .tools
            .lock()
            .unwrap()
            .values()
            .map(|tool| tool.schema())
            .collect()
    }

    pub fn list(&self) -> Vec<String> {
        let mut names: Vec<String> = self.inner.tools.lock().unwrap().keys().cloned().collect();
        names.sort();
        names
    }

    pub fn get(&self, name: &str) -> Option<Arc<ToolDefinition>> {
        self.inner.tools.lock().unwrap().get(name).cloned()
    }

    pub fn is_concurrency_safe(&self, name: &str) -> bool {
        self.get(name)
            .map(|tool| tool.is_concurrency_safe)
            .unwrap_or(false)
    }

    /// Run one accepted call through the guarded pipeline.
    pub async fn execute(
        &self,
        call_id: String,
        name: String,
        arguments: Value,
        run_ctx: ToolRunContext,
    ) -> ToolExecutionResult {
        let args = ToolCallArgs {
            call_id,
            name: name.clone(),
            arguments,
        };
        let Some(tool) = self.get(&name) else {
            return ToolExecutionResult::error(
                "UNKNOWN_TOOL",
                format!("no tool registered with name \"{name}\""),
            );
        };

        // Host-enforced rule precedes every tool hook; ask waits on an exact
        // one-use approval. Never promote the entire Agent to operate/auto.
        let mut was_approved = false;
        if let Some(agent_id) = run_ctx.agent_id.as_deref() {
            if let Some((code, reason)) = crate::runtime_controls::tool_denial(agent_id, &name) {
                if code != "APPROVAL" {
                    return ToolExecutionResult::error(code, reason);
                }
                if let Err(why) = crate::runtime_controls::request_approval(
                    agent_id,
                    &args.call_id,
                    &name,
                    &args.arguments,
                    &run_ctx.signal,
                )
                .await
                {
                    return ToolExecutionResult::error("APPROVAL", why);
                }
                was_approved = true;
            }
        }

        // 1. tools/pre-execute: allow/deny/ask decision waterfall.
        let pre_payload = json!({ "name": name, "arguments": args.arguments.clone() });
        let pre_decision = match self
            .inner
            .ctx
            .waterfall("tools/pre-execute", pre_payload, |_| {
                Box::pin(async move { Ok(json!({ "kind": "allow" })) })
            })
            .await
        {
            Ok(decision) => decision,
            Err(err) => {
                return ToolExecutionResult::error("PRE_EXECUTE", err.to_string());
            }
        };
        match pre_decision.get("kind").and_then(|k| k.as_str()) {
            Some("allow") | None => {}
            Some("deny") => {
                let reason = pre_decision
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("denied by policy");
                return ToolExecutionResult::error("DENIED", reason);
            }
            Some("ask") => {
                return ToolExecutionResult::error("APPROVAL", "tool call requires approval");
            }
            Some(_) => {}
        }

        // 2. Monotonic guards.
        let guards = self.inner.guards.lock().unwrap().clone();
        for guard in &guards {
            if let Some(reason) = guard(&args) {
                return ToolExecutionResult::error("DENIED", reason);
            }
        }

        // 3. tools/execute around-dispatch wrapper waterfall; terminal
        //    continuation runs the tool body.
        let tool_for_exec = tool.clone();
        let run_ctx_for_body = run_ctx.clone();
        let execute_payload = json!({ "name": name, "arguments": args.arguments.clone() });
        let approved_arguments = if was_approved {
            Some(args.arguments.clone())
        } else {
            None
        };
        let ctx = self.inner.ctx.clone();
        let raw_result = ctx
            .waterfall("tools/execute", execute_payload, move |payload| {
                let tool = tool_for_exec.clone();
                let args = args.clone();
                let run_ctx = run_ctx_for_body.clone();
                let approved_arguments = approved_arguments.clone();
                Box::pin(async move {
                    if let Some(expected) = approved_arguments.as_ref() {
                        if payload.get("arguments") != Some(expected) {
                            return serde_json::to_value(ToolExecutionResult::error(
                                "DENIED",
                                "approved tool arguments changed after confirmation",
                            ))
                            .map_err(|err| cordis::Error::msg(err.to_string()));
                        }
                    }
                    let result = (tool.execute)(
                        ToolCallArgs {
                            call_id: args.call_id,
                            name: args.name,
                            arguments: payload.get("arguments").cloned().unwrap_or(args.arguments),
                        },
                        run_ctx,
                    )
                    .await;
                    serde_json::to_value(&result)
                        .map_err(|err| cordis::Error::msg(format!("tool result serialize: {err}")))
                })
            })
            .await;

        let result: ToolExecutionResult = match raw_result {
            Ok(value) => match serde_json::from_value(value) {
                Ok(result) => result,
                Err(err) => {
                    return ToolExecutionResult::error("BAD_RESULT", err.to_string());
                }
            },
            Err(err) => return ToolExecutionResult::error("EXECUTE", err.to_string()),
        };

        // 4. tools/post-execute: inspect/replace the result.
        let post_payload = json!({ "name": name, "result": result });
        match self
            .inner
            .ctx
            .waterfall("tools/post-execute", post_payload, |payload| {
                Box::pin(async move { Ok(payload) })
            })
            .await
        {
            Ok(payload) => {
                match serde_json::from_value(payload.get("result").cloned().unwrap_or(Value::Null))
                {
                    Ok(result) => result,
                    Err(err) => ToolExecutionResult::error("BAD_RESULT", err.to_string()),
                }
            }
            Err(err) => ToolExecutionResult::error("POST_EXECUTE", err.to_string()),
        }
    }
}

/// The `tools` plugin: provides `ctx.tools` and registers the built-in tools.
pub fn tools_plugin() -> Arc<dyn Plugin> {
    plugin("tools", |ctx, _config: Value| async move {
        let registry = ToolRegistry::new(ctx.clone());
        let api: Arc<dyn crate::api::services::ToolRegistryApi> = Arc::new(registry.clone());
        ctx.provide(TOOLS_SERVICE, crate::api::services::ToolsService::new(api))
            .await?;
        crate::tools::builtin::register_builtin_tools(&registry)?;
        Ok(())
    })
}

impl crate::api::services::ToolRegistryApi for ToolRegistry {
    fn schemas(&self) -> Vec<crate::llm::ToolSchema> {
        self.schemas()
    }

    fn list(&self) -> Vec<String> {
        self.list()
    }

    fn is_concurrency_safe(&self, name: &str) -> bool {
        self.is_concurrency_safe(name)
    }

    fn execute(
        &self,
        call_id: String,
        name: String,
        arguments: serde_json::Value,
        run_ctx: crate::types::ToolRunContext,
    ) -> crate::api::services::BoxFuture<crate::types::ToolExecutionResult> {
        let registry = self.clone();
        Box::pin(async move { registry.execute(call_id, name, arguments, run_ctx).await })
    }

    fn register_dynamic_tool(&self, spec: crate::api::services::DynamicToolSpec) {
        self.register_dynamic_tool_with_concurrency(spec, false);
    }

    fn register_dynamic_tool_with_concurrency(
        &self,
        spec: crate::api::services::DynamicToolSpec,
        is_concurrency_safe: bool,
    ) {
        let exec = spec.exec.clone();
        let mut definition = ToolDefinition::new(
            spec.name,
            spec.description,
            spec.parameters,
            move |args: crate::types::ToolCallArgs, _run_ctx: crate::types::ToolRunContext| {
                let exec = exec.clone();
                Box::pin(async move { exec(args.arguments).await })
            },
        );
        definition.is_concurrency_safe = is_concurrency_safe;
        self.register(Arc::new(definition));
    }

    fn unregister_dynamic_tool(&self, name: &str) {
        self.unregister(name)
    }
}

#[cfg(test)]
mod approval_pipeline_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn write_tool_waits_for_exact_one_time_approval_before_body() {
        let ctx = Context::new();
        let registry = ToolRegistry::new(ctx.clone());
        let called = Arc::new(AtomicUsize::new(0));
        let hit = called.clone();
        registry.register(Arc::new(ToolDefinition::new(
            "write_file",
            "write test",
            json!({}),
            move |_args, _run_ctx| {
                let hit = hit.clone();
                Box::pin(async move {
                    hit.fetch_add(1, Ordering::SeqCst);
                    ToolExecutionResult::success_value(json!({"didWrite": true}))
                })
            },
        )));
        let agent = "approval-registry-real-execution";
        crate::runtime_controls::set(
            agent,
            crate::runtime_controls::RuntimeControls {
                permission_mode: "ask".into(),
                ..Default::default()
            },
        );
        let ctx_for_tool = ctx.clone();
        let registry_for_tool = registry.clone();
        let invocation = tokio::spawn(async move {
            registry_for_tool
                .execute(
                    "once".into(),
                    "write_file".into(),
                    json!({"path":"reviewed-target"}),
                    ToolRunContext {
                        ctx: ctx_for_tool,
                        signal: crate::types::CancelToken::new(),
                        agent_id: Some(agent.into()),
                        cwd: None,
                        allowed_roots: vec![],
                    },
                )
                .await
        });
        tokio::task::yield_now().await;
        let pending = crate::runtime_controls::pending_approvals()
            .into_iter()
            .find(|row| row.call_id == "once")
            .expect("exact call waiting");
        assert_eq!(
            called.load(Ordering::SeqCst),
            0,
            "must not write before approval"
        );
        assert_eq!(pending.arguments["path"], "reviewed-target");
        crate::runtime_controls::decide_approval(agent, &pending.id, true).unwrap();
        assert!(!invocation.await.unwrap().is_error());
        assert_eq!(called.load(Ordering::SeqCst), 1);
        assert!(crate::runtime_controls::decide_approval(agent, &pending.id, true).is_err());
        // A distinct write must ask again: the approval was not a mode switch.
        let no_approval = registry
            .execute(
                "cancelled".into(),
                "write_file".into(),
                json!({"path":"other-target"}),
                ToolRunContext {
                    ctx,
                    signal: {
                        let token = crate::types::CancelToken::new();
                        token.cancel();
                        token
                    },
                    agent_id: Some(agent.into()),
                    cwd: None,
                    allowed_roots: vec![],
                },
            )
            .await;
        assert!(no_approval.is_error());
        assert_eq!(called.load(Ordering::SeqCst), 1);
    }
}
