<script lang="ts">
  import { onMount } from "svelte";
  import { page } from "$app/state";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import Slot from "$lib/Slot.svelte";
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

  const nav = [
    { href: "/", label: "Overview", icon: "▦", key: "overview" },
    { href: "/chat", label: "Chat", icon: "◇", key: "chat" },
    { href: "/plugins", label: "Plugins", icon: "◈", key: "plugins" },
    { href: "/tools", label: "Tools", icon: "⚒", key: "tools" },
    { href: "/services", label: "Services", icon: "◇", key: "services" },
    { href: "/logs", label: "Logs", icon: "≡", key: "logs" },
    { href: "/settings", label: "Settings", icon: "⚙", key: "settings" },
    { href: "/capabilities", label: "Capabilities", icon: "✦", key: "caps" },
  ];

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
