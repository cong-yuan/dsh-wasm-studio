//! The Tauri command surface — the studio's "REST API" for the Svelte panel.
//!
//! Every command takes `State<Studio>`, does its work, and returns a serialised
//! result. Errors are `String` (Tauri requires the error type to be
//! `Serialize`; a plain message is enough for the UI to show).
//!
//! The shape mirrors a Supabase-style admin panel: **list** what exists,
//! **mutate** it, and **observe** a stream (logs and change events arrive as
//! `studio://…` events, not by polling).

use serde::Serialize;
use serde_json::Value;
use tauri::State;

use crate::studio::{Studio, StudioStatus};

/// Build the command error type from any displayable error.
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn studio_status(studio: State<'_, Studio>) -> StudioStatus {
    studio.status()
}

// ---------------------------------------------------------------------------
// Plugins (slots)
// ---------------------------------------------------------------------------

/// One row of the plugins table.
#[derive(Serialize)]
pub struct PluginRow {
    pub slot: String,
    pub plugin: String,
    /// `active` | `pending` | ...
    pub state: String,
    pub tool_count: usize,
    pub active: bool,
    /// Whether a cordis fiber is mounted for this slot.
    pub mounted: bool,
    /// Persisted desired state.
    pub enabled: bool,
    pub path: String,
    /// Services this slot declares it needs.
    pub injects: Vec<String>,
    /// Services this slot offers.
    pub provides: Vec<String>,
    /// Frontend slots this plugin **opens** for others.
    pub provides_slots: Vec<String>,
    /// Slots this plugin's UI mounts **into**.
    pub injects_slots: Vec<SlotInjectRow>,
    /// Whether this plugin ships frontend assets (an entry.js).
    pub has_ui: bool,
}

#[tauri::command]
pub fn list_plugins(studio: State<'_, Studio>) -> Vec<PluginRow> {
    let cfg = studio.config();
    studio
        .host()
        .list_plugins()
        .into_iter()
        .map(|(slot, plugin, state, tool_count, active)| {
            let (injects, provides) = studio.host().deps_of(&slot);
            let entry = cfg.plugins.get(&slot);
            // The frontend contribution, if any — surfaced here so the Plugins
            // page can show *what UI a plugin brings*, not just its tools.
            let ui = studio.host().ui_decl(&slot);
            let provides_slots = ui
                .as_ref()
                .map(|u| u.provides.iter().map(|s| s.name.clone()).collect())
                .unwrap_or_default();
            let injects_slots = ui
                .as_ref()
                .map(|u| {
                    u.injects
                        .iter()
                        .map(|i| SlotInjectRow {
                            slot: i.slot.clone(),
                            priority: i.priority,
                            component: i.component.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let has_ui = ui.map(|u| !u.assets.is_empty()).unwrap_or(false);
            PluginRow {
                mounted: studio.is_mounted(&slot),
                enabled: entry.map(|e| e.enabled).unwrap_or(false),
                path: entry.map(|e| e.path.clone()).unwrap_or_default(),
                slot,
                plugin,
                state: format!("{state:?}").to_lowercase(),
                tool_count,
                active,
                injects,
                provides,
                provides_slots,
                injects_slots,
                has_ui,
            }
        })
        .collect()
}

/// Load and mount a `.wasm` into `slot`; persists to `studio.json`.
#[tauri::command]
pub async fn load_plugin(
    studio: State<'_, Studio>,
    slot: String,
    path: String,
    config: Option<Value>,
) -> Result<(), String> {
    studio
        .mount_slot(&slot, &path, config.unwrap_or(Value::Null))
        .await
        .map_err(err)
}

/// Unload a slot, release its instance, and remove it from the config.
#[tauri::command]
pub async fn unload_plugin(studio: State<'_, Studio>, slot: String) -> Result<(), String> {
    studio.unmount_slot(&slot).await.map_err(err)
}

/// Forget a slot entirely: stop it and delete it from the config.
///
/// Distinct from [`unload_plugin`], which stops but keeps it listed.
#[tauri::command]
pub async fn remove_plugin(studio: State<'_, Studio>, slot: String) -> Result<(), String> {
    studio.remove_slot(&slot).await.map_err(err)
}

/// Enable/disable a configured slot (loads or unloads to match), persisting.
#[tauri::command]
pub async fn set_plugin_enabled(
    studio: State<'_, Studio>,
    slot: String,
    enabled: bool,
) -> Result<(), String> {
    studio.set_enabled(&slot, enabled).await.map_err(err)
}

/// Hot-reload a slot's code; returns its new tool names. A rejected build
/// leaves the running plugin intact.
#[tauri::command]
pub async fn reload_plugin(studio: State<'_, Studio>, slot: String) -> Result<Vec<String>, String> {
    studio.reload_slot(&slot).await.map_err(err)
}

/// Push a new config to a running slot (live; no restart). Returns whether the
/// guest consumed it via a `plugin_on_config` hook.
#[tauri::command]
pub async fn set_plugin_config(
    studio: State<'_, Studio>,
    slot: String,
    config: Value,
) -> Result<bool, String> {
    studio.set_slot_config(&slot, config).await.map_err(err)
}

/// Validate a `.wasm` without loading it: `(plugin_name, tool_names)`.
#[tauri::command]
pub fn validate_plugin(
    studio: State<'_, Studio>,
    path: String,
) -> Result<(String, Vec<String>), String> {
    studio.host().validate(&path).map_err(err)
}

/// Scan every candidate location for `.wasm` files not yet configured.
///
/// Returns both what was found *and* where we looked, so an empty result can
/// tell the user which directory to drop a plugin into.
#[tauri::command]
pub fn discover_plugins(studio: State<'_, Studio>) -> crate::studio::Discovery {
    studio.discover()
}

// ---------------------------------------------------------------------------
// Auto-reload
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn watch_status(studio: State<'_, Studio>) -> bool {
    studio.watching()
}

#[tauri::command]
pub fn start_watch(studio: State<'_, Studio>) {
    studio.start_watch();
}

/// Async so joining the watcher thread never blocks the UI thread.
#[tauri::command]
pub async fn stop_watch(studio: State<'_, Studio>) -> Result<(), String> {
    studio.stop_watch();
    Ok(())
}

// ---------------------------------------------------------------------------
// Frontend UI contributions
// ---------------------------------------------------------------------------

/// One plugin's frontend contribution, as the panel needs it.
#[derive(Serialize)]
pub struct UiPlugin {
    /// The plugin slot that owns this contribution.
    pub slot: String,
    /// Slots this plugin **opens** for others to fill.
    pub provides_slots: Vec<String>,
    /// Slots this plugin's UI wants to render **inside**.
    pub injects_slots: Vec<SlotInjectRow>,
    /// `{ "entry.js": "...", "style.css": "..." }` — the frontend runs these.
    pub assets: std::collections::BTreeMap<String, String>,
    /// Adjustments this plugin applies to other plugins' contributions.
    pub adjusts: Vec<UiAdjustRow>,
    /// Pages this plugin contributes, each optionally with a nav entry.
    pub routes: Vec<UiRouteRow>,
}

/// A contributed page, as the frontend receives it.
#[derive(Serialize)]
pub struct UiRouteRow {
    /// Path under the app root, e.g. `usage` or `tools/usage`.
    pub path: String,
    /// Which registered component renders it.
    pub component: String,
    /// Nav label, when the plugin asked for an entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub nav: bool,
}

/// One adjustment, as the frontend receives it.
#[derive(Serialize)]
pub struct UiAdjustRow {
    /// Glob matched against the contribution's slot.
    pub slot: String,
    /// Glob matched against the contributing plugin's id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// `hide` | `unhide` | `replace` | `priority`.
    pub action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
}

/// Where and how a plugin wants to render.
#[derive(Serialize)]
pub struct SlotInjectRow {
    pub slot: String,
    pub priority: i32,
    pub component: Option<String>,
}

/// Every loaded plugin's UI declaration.
///
/// The frontend feeds this into its `SlotRegistry`: each `provides_slots` opens
/// a slot, each `injects_slots` claims a place. Resolution is order-independent
/// and reactive (see `src/lib/slots.ts`).
#[tauri::command]
pub fn ui_contributions(studio: State<'_, Studio>) -> Vec<UiPlugin> {
    studio
        .host()
        .ui_decls()
        .into_iter()
        .map(|(slot, ui)| UiPlugin {
            slot,
            provides_slots: ui.provides.into_iter().map(|s| s.name).collect(),
            injects_slots: ui
                .injects
                .into_iter()
                .map(|i| SlotInjectRow {
                    slot: i.slot,
                    priority: i.priority,
                    component: i.component,
                })
                .collect(),
            assets: ui.assets,
            adjusts: ui
                .adjusts
                .into_iter()
                .map(|a| UiAdjustRow {
                    slot: a.slot,
                    from: a.from,
                    action: match a.action {
                        wasm_plugin_host::AdjustAction::Hide => "hide",
                        wasm_plugin_host::AdjustAction::Unhide => "unhide",
                        wasm_plugin_host::AdjustAction::Replace => "replace",
                        wasm_plugin_host::AdjustAction::Priority => "priority",
                    },
                    to: a.to,
                    by: a.by,
                    // `replace` names a component; nothing else does.
                    component: a.component,
                })
                .collect(),
            routes: ui
                .routes
                .into_iter()
                .map(|r| UiRouteRow {
                    path: r.path,
                    component: r.component,
                    title: r.title,
                    icon: r.icon,
                    nav: r.nav,
                })
                .collect(),
        })
        .collect()
}

/// Every plugin the app knows about: configured ones (running or stopped) plus
/// `.wasm` files found on disk.
///
/// The Plugins page renders this. It intentionally includes not-running
/// plugins, so stopping one does not make it vanish.
#[tauri::command]
pub fn plugin_catalog(studio: State<'_, Studio>) -> Vec<crate::studio::CatalogEntry> {
    studio.catalog()
}

// ---------------------------------------------------------------------------
// Plugin windows
// ---------------------------------------------------------------------------

/// Every window the running plugins declare, with its derived label.
#[tauri::command]
pub fn plugin_windows(studio: State<'_, Studio>) -> Vec<crate::studio::PluginWindow> {
    studio.plugin_windows()
}

/// What a given window should render.
///
/// A plugin window loads the app; on startup the frontend passes its *own*
/// window label here and gets back the plugin + component to mount, or `None`
/// for the main window.
#[tauri::command]
pub fn plugin_window_for(
    studio: State<'_, Studio>,
    label: String,
) -> Option<crate::studio::PluginWindow> {
    studio.plugin_window_by_label(&label)
}

/// Open (or focus) one of a plugin's declared windows.
#[tauri::command]
pub fn open_plugin_window(studio: State<'_, Studio>, label: String) -> Result<(), String> {
    studio.open_plugin_window(&label).map_err(err)
}

/// Open a plugin window, passing JSON params the window can read on startup.
#[tauri::command]
pub fn open_plugin_window_with(
    studio: State<'_, Studio>,
    label: String,
    params: Option<Value>,
) -> Result<(), String> {
    studio.open_plugin_window_with(&label, params).map_err(err)
}

/// Push params to an already-open window (`studio://window-params` event).
#[tauri::command]
pub fn send_window_params(
    studio: State<'_, Studio>,
    label: String,
    params: Value,
) -> Result<(), String> {
    studio.send_window_params(&label, params).map_err(err)
}

/// Close a plugin window by label.
#[tauri::command]
pub fn close_plugin_window(studio: State<'_, Studio>, label: String) -> Result<(), String> {
    studio.close_plugin_window(&label).map_err(err)
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// One row of the tools table.
#[derive(Serialize)]
pub struct ToolRow {
    pub name: String,
    pub description: String,
    pub slot: String,
    pub plugin: String,
    pub parameters: Value,
}

#[tauri::command]
pub fn list_tools(studio: State<'_, Studio>) -> Vec<ToolRow> {
    studio
        .host()
        .list_tools()
        .into_iter()
        .map(|t| ToolRow {
            name: t.name,
            description: t.description,
            slot: t.slot,
            plugin: t.plugin,
            parameters: t.parameters,
        })
        .collect()
}

/// Call a tool directly (bypassing the agent loop) with JSON args.
///
/// The guest call runs on the blocking pool: `wasmtime-wasi` 44 blocks
/// internally for WASI calls, which panics on a tokio worker thread.
#[tauri::command]
pub async fn call_tool(
    studio: State<'_, Studio>,
    tool: String,
    args: Option<Value>,
) -> Result<Value, String> {
    let args = args.unwrap_or(Value::Null);
    let host = studio.host().clone();
    tokio::task::spawn_blocking(move || host.call_tool(&tool, &args))
        .await
        .map_err(|e| format!("tool task failed: {e}"))?
        .map_err(err)
}

// ---------------------------------------------------------------------------
// Services (the inject/provide graph)
// ---------------------------------------------------------------------------

/// One node of the service graph.
#[derive(Serialize)]
pub struct ServiceRow {
    pub name: String,
    /// `wasm` when a slot provides it, `dsh` when the host does.
    pub origin: String,
    /// The providing slot, when `origin == "wasm"`.
    pub provider: Option<String>,
}

#[tauri::command]
pub fn list_services(studio: State<'_, Studio>) -> Vec<ServiceRow> {
    let host = studio.host();
    host.services()
        .into_iter()
        .map(|name| {
            let provider = host
                .registry()
                .lock()
                .ok()
                .and_then(|reg| reg.provider_of(&name));
            let origin = if provider.is_some() { "wasm" } else { "dsh" };
            ServiceRow {
                name,
                origin: origin.to_string(),
                provider,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

/// Fetch buffered logs (newest last). Live updates arrive via `studio://log`.
#[tauri::command]
pub fn get_logs(
    studio: State<'_, Studio>,
    since_seq: Option<u64>,
    slot: Option<String>,
) -> Vec<wasm_plugin_host::LogRecord> {
    let all = match slot {
        Some(s) => studio.host().logs_for(&s),
        None => studio.host().logs(),
    };
    match since_seq {
        Some(seq) => all.into_iter().filter(|r| r.seq > seq).collect(),
        None => all,
    }
}

/// Set the retained log level (`debug` | `info` | `warn` | `error`).
#[tauri::command]
pub fn set_log_level(studio: State<'_, Studio>, level: String) -> Result<String, String> {
    let level = wasm_plugin_host::LogLevel::parse(&level)
        .ok_or_else(|| format!("unknown log level `{level}`"))?;
    studio
        .host()
        .registry()
        .lock()
        .map_err(err)?
        .set_log_level(level);
    Ok(level.as_str().to_string())
}

// ---------------------------------------------------------------------------
// Agents / chat
// ---------------------------------------------------------------------------

/// Create an agent on a provider route; returns its id.
///
/// **Async on purpose.** dsh spawns the agent's driver task with a bare
/// `tokio::spawn`, so this must run inside a tokio runtime context. A
/// synchronous `#[tauri::command]` would not, and would panic with "there is no
/// reactor running".
#[tauri::command]
pub async fn create_agent(
    studio: State<'_, Studio>,
    id: Option<String>,
    provider: String,
    model: String,
    cwd: Option<String>,
) -> Result<String, String> {
    studio
        .create_agent(id, provider, model, cwd)
        .map_err(err)
}

/// Every live agent.
#[tauri::command]
pub fn list_agents(studio: State<'_, Studio>) -> Vec<crate::studio::AgentRow> {
    studio.list_agents()
}

/// Every session to show in the sidebar: live agents **and** the sessions on
/// disk that are not currently loaded.
///
/// Separate from [`list_agents`] because the two answer different questions.
/// `list_agents` is "what can I send a message to right now"; this is "what
/// exists", which is what a session list means to a user. Rows carry `live` so
/// the UI can tell them apart instead of offering a composer that will fail.
#[tauri::command]
pub fn list_sessions(studio: State<'_, Studio>) -> Vec<crate::studio::AgentRow> {
    studio.list_sessions()
}

/// Put a live agent behind a stored session so it can be continued.
///
/// Returns the session id. Idempotent: resuming an already-live session is a
/// no-op that returns the same id.
#[tauri::command]
pub fn resume_session(studio: State<'_, Studio>, session_id: String) -> Result<String, String> {
    studio.resume_session(&session_id).map_err(err)
}

/// Send a user message and wait for the turn to finish.
#[tauri::command]
pub async fn send_message(
    studio: State<'_, Studio>,
    agent_id: String,
    text: String,
    msg_id: String,
) -> Result<(), String> {
    studio.send_message(&agent_id, text, msg_id).await.map_err(err)
}

/// Queue a steer message (delivered at the next step boundary).
#[tauri::command]
pub fn steer_agent(
    studio: State<'_, Studio>,
    agent_id: String,
    text: String,
    msg_id: String,
) -> Result<(), String> {
    studio.steer(&agent_id, text, msg_id).map_err(err)
}

/// Cancel the agent's in-flight turn.
#[tauri::command]
pub fn cancel_agent(studio: State<'_, Studio>, agent_id: String) -> Result<(), String> {
    studio.cancel_agent(&agent_id).map_err(err)
}

/// Dispose an agent.
///
/// Async for the same reason as [`create_agent`]: disposal emits a session
/// event, and cordis's fire-and-forget dispatch uses `tokio::spawn`.
#[tauri::command]
pub async fn dispose_agent(studio: State<'_, Studio>, agent_id: String) -> Result<(), String> {
    studio.dispose_agent(&agent_id).map_err(err)
}

/// The full message history of an agent's session.
#[tauri::command]
pub fn transcript(
    studio: State<'_, Studio>,
    agent_id: String,
) -> Result<Vec<crate::studio::ChatMessage>, String> {
    studio.transcript(&agent_id).map_err(err)
}

// ---------------------------------------------------------------------------
// Capabilities / introspection
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct Capabilities {
    pub dsh_services: Vec<String>,
    /// The flow events WASM plugins may hook.
    pub flow_events: Vec<String>,
    /// Whether the disk compile cache is active.
    pub disk_cache: bool,
}


/// Persisted LLM providers (`extra.llm`) plus live registered route names.
#[tauri::command]
pub fn get_llm_config(studio: State<'_, Studio>) -> serde_json::Value {
    studio.llm_config()
}

/// Merge a patch into `extra.llm` and write `studio.json`.
///
/// New provider routes are not live until Studio restarts (`restart_required`).
#[tauri::command]
pub fn set_llm_config(
    studio: State<'_, Studio>,
    patch: serde_json::Value,
) -> Result<serde_json::Value, String> {
    studio.set_llm_config(patch).map_err(err)
}

#[tauri::command]
pub fn capabilities(studio: State<'_, Studio>) -> Capabilities {
    Capabilities {
        dsh_services: crate::studio::dsh_services()
            .into_iter()
            .map(str::to_string)
            .collect(),
        flow_events: wasm_plugin_host::FlowEvent::ALL
            .iter()
            .map(|e| e.as_str().to_string())
            .collect(),
        disk_cache: studio
            .host()
            .registry()
            .lock()
            .map(|r| r.runtime().disk_cache_enabled())
            .unwrap_or(false),
    }
}
