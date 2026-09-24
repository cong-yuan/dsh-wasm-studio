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
  import { onDestroy, onMount } from "svelte";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { pluginWindowFor, errorMessage, type PluginWindow } from "$lib/api";
  import { pluginHost } from "$lib/plugin-runtime";

  let spec = $state<PluginWindow | null>(null);
  let error = $state<string | null>(null);
  let ready = $state(false);
  let container: HTMLElement | undefined = $state();
  let dispose: (() => void) | null = null;

  // Plugin watcher reloads replace handles and tear down their live mounts.
  // Re-run this component's mount effect after every host sync so a plugin
  // window cannot stay blank after its WASM is rebuilt.
  let revision = $state(0);
  const unsubscribe = pluginHost.subscribe(() => (revision += 1));
  onDestroy(unsubscribe);

  onMount(async () => {
    try {
      // Our own label is our identity — the backend maps it to a plugin.
      const label = getCurrentWindow().label;
      spec = await pluginWindowFor(label);
      if (!spec) {
        error = `no plugin declares a window labelled “${label}”`;
        return;
      }
      // An `html` window is injected by the backend before this page would
      // ever render; reaching here means the injection was skipped.
      if (spec.content === "html") {
        error = `“${label}” is a standalone HTML window — it should have been injected`;
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
    void revision;
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
    // `fill` because a window component owns the window: its `height: 100%`
    // needs to resolve against something with a real height.
    dispose = pluginHost.mountComponent(s.slot, s.component, `__window__${s.label}`, el, {
      fill: true,
    });
    return () => {
      dispose?.();
      dispose = null;
    };
  });
</script>

<!--
  A plugin window is the plugin's own surface. The host contributes NOTHING
  here — no header, no padding, no background — because a plugin that declares
  a window is drawing the whole thing itself (see `lib/slots.js` in the shell).

  This used to wrap the plugin in a `win-head` bar naming `slot → component`
  plus 16px of padding on a `--bg-canvas` background. The result was a black
  frame around every plugin window and a strip of host chrome above it. Both
  were host rendering choices the plugin could not override, and both are gone.

  If you need to know what a window renders, its title says it, and the Plugins
  page lists every window with its owner. A strip of debug chrome is not worth
  a visible defect in every window.
-->
{#if error}
  <div class="notice err">{error}</div>
{:else if !spec}
  <div class="empty">loading plugin window…</div>
{:else}
  <div class="win-body" bind:this={container}></div>
{/if}

<style>
  /* The plugin owns the window: fill it, add nothing, and let the plugin's own
     stylesheet decide the background. `100%` rather than `100vh` so the chain
     from the layout is what sets the size — a second `100vh` here would be a
     different number to debug if a scrollbar appears. */
  .win-body {
    height: 100%;
    overflow: auto;
  }
  .notice,
  .empty {
    margin: 24px;
  }
</style>
