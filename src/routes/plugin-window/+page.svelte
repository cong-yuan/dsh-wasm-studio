<script lang="ts">
  /**
   * A plugin window: the app, but showing one plugin component full-window.
   *
   * The window is labelled `plugin-<slot>-<name>`. On mount it asks the backend
   * what that label means, then mounts the named component with the same
   * `PluginHost` the main window uses — so a plugin renders identically here and
   * in a slot; only the mount point differs.
   *
   * Nothing else of the app is shown (no sidebar, no nav) — see +layout.svelte,
   * which detects a plugin window and renders this route alone.
   */
  import { onMount } from "svelte";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { pluginWindowFor, errorMessage, type PluginWindow } from "$lib/api";
  import { pluginHost } from "$lib/plugin-runtime";

  let spec = $state<PluginWindow | null>(null);
  let error = $state<string | null>(null);
  let ready = $state(false);
  let container: HTMLElement | undefined = $state();
  let dispose: (() => void) | null = null;

  onMount(async () => {
    try {
      // Our own label is our identity — the backend maps it to a plugin.
      const label = getCurrentWindow().label;
      spec = await pluginWindowFor(label);
      if (!spec) {
        error = `no plugin declares a window labelled “${label}”`;
        return;
      }
      // The plugin's entry.js must have run before its component exists.
      await pluginHost.sync();
      ready = true;
    } catch (e) {
      error = errorMessage(e);
    }
  });

  // Mount the component once the host is synced and the element exists.
  $effect(() => {
    const s = spec;
    const el = container;
    if (!s || !el || !ready) return;

    const factory = pluginHost.factoryForComponent(s.slot, s.component);
    el.replaceChildren();
    if (!factory) {
      el.innerHTML = `<div class="empty">component “${s.component}” is not registered by “${s.slot}”</div>`;
      return;
    }
    dispose?.();
    dispose = pluginHost.mountComponent(s.slot, s.component, `__window__${s.label}`, el);
    return () => {
      dispose?.();
      dispose = null;
    };
  });
</script>

<div class="plugin-window">
  {#if error}
    <div class="notice err" style="margin: 24px;">{error}</div>
  {:else if !spec}
    <div class="empty" style="margin: 24px;">loading plugin window…</div>
  {:else}
    <header class="win-head">
      <span class="mono faint" style="font-size: 11px;">
        {spec.slot} → {spec.component}
      </span>
      <span class="spacer"></span>
    </header>
    <div class="win-body" bind:this={container}></div>
  {/if}
</div>

<style>
  .plugin-window {
    display: flex;
    flex-direction: column;
    height: 100vh;
    background: var(--bg-canvas);
    color: var(--text);
  }
  .win-head {
    display: flex;
    align-items: center;
    padding: 8px 14px;
    border-bottom: 1px solid var(--border);
    background: var(--bg-panel);
  }
  .win-body {
    flex: 1;
    overflow: auto;
    padding: 16px;
  }
</style>
