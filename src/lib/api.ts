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
  slot_count: number;
  /** Tools contributed by WASM plugins. */
  wasm_tool_count: number;
  /** Total tools on the harness registry (WASM + dsh built-ins). */
  tool_count: number;
  service_count: number;
}

/** One row of the plugins table (mirrors `commands::PluginRow`). */
export interface PluginRow {
  slot: string;
  plugin: string;
  state: string;
  tool_count: number;
  active: boolean;
  mounted: boolean;
  injects: string[];
  provides: string[];
}

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

export const reloadPlugin = (slot: string, path?: string) =>
  invoke<void>("reload_plugin", { slot, path: path ?? null });

export const setPluginConfig = (slot: string, config: unknown) =>
  invoke<boolean>("set_plugin_config", { slot, config });

export const validatePlugin = (path: string) =>
  invoke<[string, string[]]>("validate_plugin", { path });

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
