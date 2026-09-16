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
}

/** A `.wasm` found on disk but not yet configured (mirrors `studio::Discovered`). */
export interface Discovered {
  slot: string;
  path: string;
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

export const unloadPlugin = (slot: string) =>
  invoke<void>("unload_plugin", { slot });

/** Enable or disable a configured slot (loads/unloads to match); persists. */
export const setPluginEnabled = (slot: string, enabled: boolean) =>
  invoke<void>("set_plugin_enabled", { slot, enabled });

/** Hot-reload a slot; returns its new tool names. A rejected build is an error
 *  and leaves the running plugin intact. */
export const reloadPlugin = (slot: string) =>
  invoke<string[]>("reload_plugin", { slot });

/** `.wasm` files in the plugins dir that are not yet configured. */
export const discoverPlugins = () => invoke<Discovered[]>("discover_plugins");

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
  status: "idle" | "running";
  messages: number;
  turns: number;
  busy: boolean;
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
