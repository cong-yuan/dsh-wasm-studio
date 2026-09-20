//! Typed wrappers over the Rust command surface.
//!
//! Every backend command is exposed through one function here, so the Svelte
//! pages never call `invoke` with a bare string — a typo becomes a compile
//! error, and the return types line up with the Rust `#[derive(Serialize)]`
//! structs.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/** Headline counters + paths (mirrors `studio::StudioStatus`). */
export interface StudioStatus {
  booted: boolean;
  plugins_dir: string;
  config_path: string;
  slot_count: number;
  /** Tools contributed by WASM plugins. */
  wasm_tool_count: number;
  /** Total tools on the harness registry (WASM + dsh built-ins). */
  tool_count: number;
  service_count: number;
  /** Whether the auto-reload watcher is running. */
  watching: boolean;
}

/** One row of the plugins table (mirrors `commands::PluginRow`). */
export interface PluginRow {
  slot: string;
  plugin: string;
  state: string;
  tool_count: number;
  active: boolean;
  mounted: boolean;
  /** Persisted desired state. */
  enabled: boolean;
  path: string;
  injects: string[];
  provides: string[];
  /** Frontend slots this plugin opens for others. */
  provides_slots: string[];
  /** Slots this plugin's UI mounts into. */
  injects_slots: SlotInjectRow[];
  /** Whether the plugin ships frontend assets (an entry.js). */
  has_ui: boolean;
}

/** A `.wasm` found on disk but not yet configured (mirrors `studio::Discovered`). */
export interface Discovered {
  slot: string;
  path: string;
}

/** One directory consulted during discovery (mirrors `studio::SearchedRoot`). */
export interface SearchedRoot {
  path: string;
  label: string;
  exists: boolean;
  wasm_count: number;
}

/** One entry of the plugin catalog (mirrors `studio::CatalogEntry`). */
export interface CatalogEntry {
  slot: string;
  plugin: string;
  path: string;
  /** Is a guest instance loaded right now? */
  running: boolean;
  /** Is it active (injects satisfied)? */
  active: boolean;
  /** `active` | `pending` | `stopped` | `available` | ... */
  state: string;
  tool_count: number;
  in_config: boolean;
  enabled: boolean;
  exists: boolean;
}

/** A discovery scan result (mirrors `studio::Discovery`). */
export interface Discovery {
  plugins: Discovered[];
  /** Every directory we looked in, so an empty result is explainable. */
  searched: SearchedRoot[];
}

/** A change event from the backend (mirrors `studio::StudioEvent`). */
export type StudioEvent =
  | { kind: "reloaded"; slot: string; tools: string[] }
  | { kind: "reload_failed"; slot: string; error: string }
  | { kind: "changed" };

/** One row of the tools table (mirrors `commands::ToolRow`). */
export interface ToolRow {
  name: string;
  description: string;
  slot: string;
  plugin: string;
  parameters: unknown;
}

/** One node of the service graph (mirrors `commands::ServiceRow`). */
export interface ServiceRow {
  name: string;
  origin: "wasm" | "dsh";
  provider: string | null;
}

/** A captured guest log line (mirrors `wasm_plugin_host::LogRecord`). */
export interface LogRecord {
  seq: number;
  slot: string;
  plugin: string;
  level: "debug" | "info" | "warn" | "error";
  message: string;
}

/** What the studio can do (mirrors `commands::Capabilities`). */
export interface Capabilities {
  dsh_services: string[];
  flow_events: string[];
  disk_cache: boolean;
}

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------

export const studioStatus = () => invoke<StudioStatus>("studio_status");

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

export const listPlugins = () => invoke<PluginRow[]>("list_plugins");

export const loadPlugin = (slot: string, path: string, config?: unknown) =>
  invoke<void>("load_plugin", { slot, path, config: config ?? null });

/** Stop a slot: release its instance but keep it listed (startable again). */
export const unloadPlugin = (slot: string) =>
  invoke<void>("unload_plugin", { slot });

/** Forget a slot entirely: stop it and delete it from the config. */
export const removePlugin = (slot: string) =>
  invoke<void>("remove_plugin", { slot });

/** Enable or disable a configured slot (loads/unloads to match); persists. */
export const setPluginEnabled = (slot: string, enabled: boolean) =>
  invoke<void>("set_plugin_enabled", { slot, enabled });

/** Hot-reload a slot; returns its new tool names. A rejected build is an error
 *  and leaves the running plugin intact. */
export const reloadPlugin = (slot: string) =>
  invoke<string[]>("reload_plugin", { slot });

/** Every plugin the app knows about (running or not). */
export const pluginCatalog = () => invoke<CatalogEntry[]>("plugin_catalog");

/** Scan every candidate location for `.wasm` files not yet configured. */
export const discoverPlugins = () => invoke<Discovery>("discover_plugins");

export const setPluginConfig = (slot: string, config: unknown) =>
  invoke<boolean>("set_plugin_config", { slot, config });

export const validatePlugin = (path: string) =>
  invoke<[string, string[]]>("validate_plugin", { path });

// ---------------------------------------------------------------------------
// Auto-reload
// ---------------------------------------------------------------------------

export const watchStatus = () => invoke<boolean>("watch_status");
export const startWatch = () => invoke<void>("start_watch");
export const stopWatch = () => invoke<void>("stop_watch");

// ---------------------------------------------------------------------------
// Plugin windows
// ---------------------------------------------------------------------------

/** A window a plugin declares (mirrors `studio::PluginWindow`). */
export interface PluginWindow {
  label: string;
  slot: string;
  name: string;
  component: string;
  title: string;
  width: number;
  height: number;
  /** `startup` (owns the launch view), `auto` (opens on activate), or `manual`. */
  open: string;
  /** `app` (render the declared component) or `html` (a standalone page). */
  content: "app" | "html";
}

/** Every window the running plugins declare. */
export const pluginWindows = () => invoke<PluginWindow[]>("plugin_windows");

/** What a given window label should render (null for the main window). */
export const pluginWindowFor = (label: string) =>
  invoke<PluginWindow | null>("plugin_window_for", { label });

/** Open (or focus) a plugin window by label. */
export const openPluginWindow = (label: string) =>
  invoke<void>("open_plugin_window", { label });

/** Open a plugin window, passing JSON the window reads on startup. */
export const openPluginWindowWith = (label: string, params?: unknown) =>
  invoke<void>("open_plugin_window_with", { label, params: params ?? null });

/** Push params to an already-open window (delivered as studio://window-params). */
export const sendWindowParams = (label: string, params: unknown) =>
  invoke<void>("send_window_params", { label, params });

/** Close a plugin window. */
export const closePluginWindow = (label: string) =>
  invoke<void>("close_plugin_window", { label });

// ---------------------------------------------------------------------------
// Frontend UI contributions
// ---------------------------------------------------------------------------

/** Where and how a plugin wants to render (mirrors `commands::SlotInjectRow`). */
export interface SlotInjectRow {
  slot: string;
  priority: number;
  component: string | null;
}

/** One plugin's frontend contribution (mirrors `commands::UiPlugin`). */
export interface UiPlugin {
  slot: string;
  provides_slots: string[];
  injects_slots: SlotInjectRow[];
  assets: Record<string, string>;
  /** Adjustments this plugin applies to other plugins' contributions. */
  adjusts?: UiAdjustRow[];
  /** Pages this plugin contributes, each optionally with a nav entry. */
  routes?: UiRouteRow[];
}

/** A contributed page, as the backend reports it. */
export interface UiRouteRow {
  /** Path under the app root, e.g. `usage` (no leading slash). */
  path: string;
  /** Which registered component renders it. */
  component: string;
  /** Nav label, when the plugin asked for an entry. */
  title?: string;
  icon?: string;
  /** Whether to show a sidebar entry (defaults to true backend-side). */
  nav: boolean;
}

/** One adjustment as the backend reports it. */
export interface UiAdjustRow {
  slot?: string;
  from?: string;
  action: "hide" | "unhide" | "replace" | "priority";
  to?: number;
  by?: number;
  component?: string;
}

/** Every loaded plugin's UI declaration. */
export const uiContributions = () =>
  invoke<UiPlugin[]>("ui_contributions");

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

export const listTools = () => invoke<ToolRow[]>("list_tools");

export const callTool = (tool: string, args?: unknown) =>
  invoke<unknown>("call_tool", { tool, args: args ?? null });

// ---------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------

export const listServices = () => invoke<ServiceRow[]>("list_services");

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

export const getLogs = (sinceSeq?: number, slot?: string) =>
  invoke<LogRecord[]>("get_logs", {
    sinceSeq: sinceSeq ?? null,
    slot: slot ?? null,
  });

export const setLogLevel = (level: string) =>
  invoke<string>("set_log_level", { level });

/** Subscribe to the live guest-log stream. Returns the unlisten function. */
export const onLog = (
  handler: (rec: LogRecord) => void,
): Promise<UnlistenFn> =>
  listen<LogRecord>("studio://log", (event) => handler(event.payload));

/** Subscribe to plugin change events (load/unload/reload/config). */
export const onPluginChanged = (
  handler: (ev: StudioEvent) => void,
): Promise<UnlistenFn> =>
  listen<StudioEvent>("studio://plugins-changed", (event) =>
    handler(event.payload),
  );

/** Subscribe to the generic "something changed, refetch" signal. */
export const onChanged = (handler: () => void): Promise<UnlistenFn> =>
  listen("studio://changed", () => handler());

// ---------------------------------------------------------------------------
// Agents / chat
// ---------------------------------------------------------------------------

/** One agent as the UI sees it (mirrors `studio::AgentRow`). */
export interface AgentRow {
  id: string;
  /**
   * `stored` means the session is on disk with no driver behind it yet:
   * readable, but not sendable until resumed.
   */
  status: "idle" | "running" | "stored";
  /**
   * Whether a driver is behind this row right now. A `false` row came from the
   * store on disk; opening it is read-only until `resumeSession` succeeds.
   */
  live: boolean;
  messages: number;
  turns: number;
  busy: boolean;
  /** First user message, truncated — a label, not the id. */
  title: string;
  /** Cumulative usage, when the provider reported it. */
  usage: TokenUsageRow;
}

/** Cumulative per-session token accounting (mirrors `studio::TokenUsageRow`). */
export interface TokenUsageRow {
  input: number;
  output: number;
  cache_read?: number;
  cache_write?: number;
  reasoning?: number;
  /** Model calls these totals span; `0` means nothing was reported. */
  calls: number;
}

/** One message in a chat transcript (mirrors `studio::ChatMessage`). */
export interface ChatMessage {
  role: "user" | "assistant" | "system";
  text: string;
  reasoning: string;
  tool_calls: ChatToolCall[];
  tool_results: ChatToolResult[];
}

export interface ChatToolCall {
  id: string;
  name: string;
  arguments: string;
}

export interface ChatToolResult {
  tool_call_id: string;
  content: string;
  is_error: boolean;
}

export const createAgent = (
  provider: string,
  model: string,
  cwd?: string,
  id?: string,
) =>
  invoke<string>("create_agent", {
    id: id ?? null,
    provider,
    model,
    cwd: cwd ?? null,
  });

export const listAgents = () => invoke<AgentRow[]>("list_agents");

/**
 * Every session for the list: live agents **and** the sessions on disk.
 *
 * Distinct from `listAgents` on purpose — that answers "what can I send to
 * right now", this answers "what exists", which is what a session list means.
 * Each row carries `live` so the two can be told apart.
 */
export const listSessions = () => invoke<AgentRow[]>("list_sessions");

/** Put a live agent behind a stored session so it can be continued. */
export const resumeSession = (sessionId: string) =>
  invoke<string>("resume_session", { sessionId });

export const sendMessage = (agentId: string, text: string, msgId: string) =>
  invoke<void>("send_message", { agentId, text, msgId });

export const steerAgent = (agentId: string, text: string, msgId: string) =>
  invoke<void>("steer_agent", { agentId, text, msgId });

export const cancelAgent = (agentId: string) =>
  invoke<void>("cancel_agent", { agentId });

export const disposeAgent = (agentId: string) =>
  invoke<void>("dispose_agent", { agentId });

export const transcript = (agentId: string) =>
  invoke<ChatMessage[]>("transcript", { agentId });

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

export const capabilities = () => invoke<Capabilities>("capabilities");

/** Normalise an unknown thrown value into a message the UI can show. */
export function errorMessage(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return JSON.stringify(e);
}
