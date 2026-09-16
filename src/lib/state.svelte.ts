// Shared reactive app state, using Svelte 5 runes.
//
// A tiny store rather than a framework: the panel is small enough that a few
// rune-backed functions are clearer than a state library. Pages import these
// and read them reactively.

import {
  capabilities,
  errorMessage,
  listPlugins,
  listServices,
  listTools,
  onChanged,
  onPluginChanged,
  studioStatus,
  type Capabilities,
  type PluginRow,
  type ServiceRow,
  type StudioStatus,
  type StudioEvent,
  type ToolRow,
} from "$lib/api";

/** Headline status. `null` until first load. */
let status = $state<StudioStatus | null>(null);
/** Plugin table rows. */
let plugins = $state<PluginRow[]>([]);
/** Tool table rows. */
let tools = $state<ToolRow[]>([]);
/** Service graph rows. */
let services = $state<ServiceRow[]>([]);
/** Backend capabilities, loaded once. */
let caps = $state<Capabilities | null>(null);
/** Last error from a refresh, surfaced in the UI. */
let lastError = $state<string | null>(null);
/** Whether a refresh is in flight. */
let loading = $state(false);

export function getStatus() {
  return status;
}
export function getPlugins() {
  return plugins;
}
export function getTools() {
  return tools;
}
export function getServices() {
  return services;
}
export function getCaps() {
  return caps;
}
export function getLastError() {
  return lastError;
}
export function isLoading() {
  return loading;
}

/** Reload everything the dashboard shows. */
export async function refreshAll(): Promise<void> {
  loading = true;
  lastError = null;
  try {
    const [s, p, t, svc] = await Promise.all([
      studioStatus(),
      listPlugins(),
      listTools(),
      listServices(),
    ]);
    status = s;
    plugins = p;
    tools = t;
    services = svc;
  } catch (e) {
    lastError = errorMessage(e);
  } finally {
    loading = false;
  }
}

/** Load capabilities once (they do not change at runtime). */
export async function ensureCapabilities(): Promise<void> {
  if (caps) return;
  try {
    caps = await capabilities();
  } catch (e) {
    lastError = errorMessage(e);
  }
}

export function setError(msg: string | null) {
  lastError = msg;
}

// ---------------------------------------------------------------------------
// Live wiring
// ---------------------------------------------------------------------------

/** Recent backend change events, newest first (shown as a feed). */
let events = $state<StudioEvent[]>([]);
export function getEvents() {
  return events;
}

let wired = false;

/**
 * Subscribe to backend events once. A hot reload performed by the watcher (with
 * no UI action) still lands here, so the panel stays in step without polling.
 */
export async function wireEvents(): Promise<void> {
  if (wired) return;
  wired = true;
  await onPluginChanged((ev) => {
    events = [ev, ...events].slice(0, 50);
    // Any change means the tables may be stale.
    refreshAll();
  });
  await onChanged(() => {
    refreshAll();
  });
}
