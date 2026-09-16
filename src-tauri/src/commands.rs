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

use crate::studio::{Discovered, Studio, StudioStatus};

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
    studio.set_slot_config(&slot, config).map_err(err)
}

/// Validate a `.wasm` without loading it: `(plugin_name, tool_names)`.
#[tauri::command]
pub fn validate_plugin(
    studio: State<'_, Studio>,
    path: String,
) -> Result<(String, Vec<String>), String> {
    studio.host().validate(&path).map_err(err)
}

/// Scan the plugins directory for `.wasm` files not yet configured.
#[tauri::command]
pub fn discover_plugins(studio: State<'_, Studio>) -> Vec<Discovered> {
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

#[tauri::command]
pub fn stop_watch(studio: State<'_, Studio>) {
    studio.stop_watch();
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
#[tauri::command]
pub async fn call_tool(
    studio: State<'_, Studio>,
    tool: String,
    args: Option<Value>,
) -> Result<Value, String> {
    let args = args.unwrap_or(Value::Null);
    studio.host().call_tool(&tool, &args).map_err(err)
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
