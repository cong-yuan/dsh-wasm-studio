//! The Tauri command surface — the studio's "REST API" for the Svelte panel.
//!
//! Every command takes `State<Studio>`, does its work, and returns a serialised
//! result. Errors are `String` (Tauri requires the error type to be
//! `Serialize`; a plain message is enough for the UI to show).
//!
//! The shape mirrors a Supabase-style admin panel: **list** what exists,
//! **mutate** it, and **observe** a stream (logs arrive as `studio://log`
//! events, not by polling).

use serde::Serialize;
use serde_json::Value;
use tauri::State;

use crate::studio::{Studio, StudioStatus};

/// Build the `#[tauri::command]` error type from any `anyhow` error.
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------

/// Headline counters + paths, for the dashboard header.
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
    /// `active` | `pending` | `failed` | ...
    pub state: String,
    pub tool_count: usize,
    pub active: bool,
    /// Whether a cordis fiber is mounted for this slot.
    pub mounted: bool,
    /// Services this slot declares it needs.
    pub injects: Vec<String>,
    /// Services this slot offers.
    pub provides: Vec<String>,
}

#[tauri::command]
pub fn list_plugins(studio: State<'_, Studio>) -> Vec<PluginRow> {
    studio
        .host()
        .list_plugins()
        .into_iter()
        .map(|(slot, plugin, state, tool_count, active)| {
            let (injects, provides) = studio.host().deps_of(&slot);
            PluginRow {
                mounted: studio.is_mounted(&slot),
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

/// Load and mount a `.wasm` into `slot`. Replaces an existing slot of that name.
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

/// Unload a slot and drop its cordis fiber (releasing the wasm instance).
#[tauri::command]
pub async fn unload_plugin(studio: State<'_, Studio>, slot: String) -> Result<(), String> {
    studio.unmount_slot(&slot).await.map_err(err)
}

/// Hot-reload a slot's code from its `.wasm` (atomic; a broken build is rejected).
#[tauri::command]
pub async fn reload_plugin(
    studio: State<'_, Studio>,
    slot: String,
    path: Option<String>,
) -> Result<(), String> {
    let path = match path {
        Some(p) => p,
        None => studio
            .host()
            .registry()
            .lock()
            .map_err(err)?
            .slot_path(&slot)
            .map(|p| p.display().to_string())
            .ok_or_else(|| format!("slot `{slot}` is not loaded and no path was given"))?,
    };
    studio.reload_slot(&slot, &path).map_err(err)
}

/// Push a new config to a running slot (live; no restart).
#[tauri::command]
pub async fn set_plugin_config(
    studio: State<'_, Studio>,
    slot: String,
    config: Value,
) -> Result<bool, String> {
    studio.host().apply_config(&slot, config).map_err(err)
}

/// Validate a `.wasm` without loading it: `(plugin_name, tool_names)`.
#[tauri::command]
pub fn validate_plugin(studio: State<'_, Studio>, path: String) -> Result<(String, Vec<String>), String> {
    studio.host().validate(&path).map_err(err)
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

/// One node/edge of the service graph.
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
    // A service is "dsh" when the host declared it external; otherwise the
    // registry resolved it to a slot.
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
pub fn get_logs(studio: State<'_, Studio>, since_seq: Option<u64>, slot: Option<String>) -> Vec<wasm_plugin_host::LogRecord> {
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
    studio.host().registry().lock().map_err(err)?.set_log_level(level);
    Ok(level.as_str().to_string())
}

// ---------------------------------------------------------------------------
// Capabilities / introspection
// ---------------------------------------------------------------------------

/// What this studio can do, so the UI can show a "capabilities" panel.
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
