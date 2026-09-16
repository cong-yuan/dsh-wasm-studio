// Shared reactive app state, using Svelte 5 runes.
//
// A tiny store rather than a framework: the panel is small enough that a few
// rune-backed functions are clearer than a state library. Pages import these
// and read them reactively.

import { pluginHost } from "$lib/plugin-runtime";
import {
  capabilities,
  errorMessage,
  listPlugins,
  pluginCatalog,
  listServices,
  listTools,
  onChanged,
  onPluginChanged,
  studioStatus,
  type Capabilities,
  type CatalogEntry,
  type PluginRow,
  type ServiceRow,
  type StudioStatus,
  type StudioEvent,
  type ToolRow,
} from "$lib/api";

/** Headline status. `null` until first load. */
let status = $state<StudioStatus | null>(null);
/** Plugin table rows (running plugins only). */
let plugins = $state<PluginRow[]>([]);
/** Every known plugin, running or not. */
let catalog = $state<CatalogEntry[]>([]);
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
export function getCatalog() {
  return catalog;
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
    const [s, p, cat, t, svc] = await Promise.all([
      studioStatus(),
      listPlugins(),
      pluginCatalog(),
      listTools(),
      listServices(),
    ]);
    status = s;
    plugins = p;
    catalog = cat;
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
    // Any change means the tables may be stale, and the plugin set may have
    // changed — so resync the frontend slot registry too.
    refreshAll();
    pluginHost.sync();
  });
  await onChanged(() => {
    refreshAll();
    pluginHost.sync();
  });
  // Initial sync so plugins already loaded show up on first paint.
  pluginHost.sync();
}
