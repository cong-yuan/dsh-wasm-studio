<script lang="ts">
  import { onMount } from "svelte";
  import { page } from "$app/state";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import Slot from "$lib/Slot.svelte";
  import { pluginHost } from "$lib/plugin-runtime";
  import {
    refreshAll,
    wireEvents,
    getStatus,
    getPlugins,
    getTools,
    getServices,
  } from "$lib/state.svelte";
  import "../lib/theme.css";

  let { children } = $props();

  // A plugin window (`plugin-*`) shows only its own content — no sidebar.
  // If we are not running inside Tauri (e.g. a plain dev server), the window
  // API would throw, so treat that as the main window.
  let isPluginWindow = $state(false);
  onMount(() => {
    try {
      isPluginWindow = getCurrentWindow().label.startsWith("plugin-");
    } catch {
      isPluginWindow = false;
    }
  });

  // The app's own pages. Plugins add more via `ui.routes`, and those come
  // through the same list so ordering and the active state behave identically —
  // a contributed page is a page, not a special case.
  const builtinNav = [
    { href: "/", label: "Overview", icon: "▦", key: "overview" },
    { href: "/chat", label: "Chat", icon: "◇", key: "chat" },
    { href: "/plugins", label: "Plugins", icon: "◈", key: "plugins" },
    { href: "/tools", label: "Tools", icon: "⚒", key: "tools" },
    { href: "/services", label: "Services", icon: "◇", key: "services" },
    { href: "/logs", label: "Logs", icon: "≡", key: "logs" },
    { href: "/settings", label: "Settings", icon: "⚙", key: "settings" },
    { href: "/capabilities", label: "Capabilities", icon: "✦", key: "caps" },
  ];

  // Plugin-contributed nav entries, kept live. A route declared with
  // `nav: false` contributes a page with no sidebar entry, which is legitimate
  // (a detail view reached from another page).
  let navRevision = $state(0);
  pluginHost.subscribe(() => (navRevision += 1));
  const pluginNav = $derived.by(() => {
    void navRevision;
    return pluginHost.slots.navRoutes().map((r) => ({
      href: `/${r.path}`,
      label: r.title ?? r.path,
      icon: r.icon ?? "◆",
      key: `plugin:${r.path}`,
      owner: r.owner,
    }));
  });
  const nav = $derived([...builtinNav, ...pluginNav]);

  // Refresh once on mount, then keep the sidebar counters live.
  onMount(() => {
    refreshAll();
    // Live updates arrive as backend events; the interval is only a slow
    // safety net for anything that does not emit one.
    wireEvents();
    const t = setInterval(refreshAll, 8000);
    return () => clearInterval(t);
  });

  let current = $derived(page.url.pathname);

  function countFor(key: string): number | null {
    if (key === "plugins") return getPlugins().length;
    if (key === "tools") return getTools().length;
    if (key === "services") return getServices().length;
    return null;
  }
</script>

{#if isPluginWindow}
  <!-- A plugin window: render only the route, filling the window. -->
  <div class="plugin-window-root">
    {@render children()}
  </div>
{:else}
<div class="shell">
  <aside class="sidebar">
    <div class="brand">
      <span class="dot"></span>
      <span>WASM Studio</span>
    </div>

    {#each nav as item}
      <a href={item.href} class="nav-item" class:active={current === item.href}>
        <span>{item.icon}</span>
        <span>{item.label}</span>
        {#if countFor(item.key) !== null}
          <span class="count">{countFor(item.key)}</span>
        {/if}
      </a>
    {/each}

    <div class="spacer"></div>
    <Slot slot="sidebar.items" />

    {#if getStatus()}
      <div class="faint" style="font-size: 11px; padding: 0 10px;">
        {#if getStatus()?.booted}
          <span style="color: var(--ok)">●</span> harness online
        {:else}
          <span style="color: var(--warn)">●</span> booting…
        {/if}
        <br />
        {#if getStatus()?.watching}
          <span style="color: var(--info)">◉</span> auto-reload on
        {:else}
          <span style="color: var(--text-faint)">○</span> auto-reload off
        {/if}
      </div>
    {/if}
  </aside>

  <main class="main">
    {@render children()}
  </main>
</div>
{/if}

<style>
  .plugin-window-root {
    height: 100vh;
  }
</style>
