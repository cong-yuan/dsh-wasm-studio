<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import type { UnlistenFn } from "@tauri-apps/api/event";
  import { getLogs, onLog, setLogLevel, errorMessage, type LogRecord } from "$lib/api";

  /** Cap the in-memory console so a chatty plugin cannot grow it unbounded. */
  const MAX_LINES = 2000;

  let lines = $state<LogRecord[]>([]);
  let level = $state<string>("debug");
  let filter = $state<string>("");
  let paused = $state(false);
  let unlisten: UnlistenFn | null = null;
  let autoScroll = $state(true);
  let consoleEl = $state<HTMLElement | null>(null);

  onMount(async () => {
    // Seed from the backend buffer, then switch to the live stream.
    try {
      const initial = await getLogs();
      lines = initial.slice(-MAX_LINES);
    } catch (e) {
      console.error(errorMessage(e));
    }
    unlisten = await onLog((rec) => {
      if (paused) return;
      lines = [...lines.slice(-(MAX_LINES - 1)), rec];
      if (autoScroll && consoleEl) {
        queueMicrotask(() => consoleEl?.scrollTo(0, consoleEl.scrollHeight));
      }
    });
  });

  onDestroy(() => unlisten?.());

  let shown = $derived(
    filter.trim()
      ? lines.filter(
          (l) =>
            l.message.toLowerCase().includes(filter.toLowerCase()) ||
            l.slot.toLowerCase().includes(filter.toLowerCase()),
        )
      : lines,
  );

  async function changeLevel(l: string) {
    level = l;
    try {
      await setLogLevel(l);
    } catch (e) {
      console.error(errorMessage(e));
    }
  }
</script>

<div class="page-head">
  <div>
    <h1>Logs</h1>
    <div class="sub">
      Captured from guest WASI stdout/stderr — any language, no glue. Streams
      live over <code>studio://log</code>.
    </div>
  </div>
  <div class="toolbar">
    <button class="ghost" onclick={() => (paused = !paused)}>
      {paused ? "▶ resume" : "⏸ pause"}
    </button>
    <button class="ghost" onclick={() => (lines = [])}>clear</button>
  </div>
</div>

<div class="toolbar" style="margin-bottom: 12px;">
  <div style="width: 220px;">
    <input bind:value={filter} placeholder="filter by text or slot…" />
  </div>
  <div style="width: 130px;">
    <select value={level} onchange={(e) => changeLevel(e.currentTarget.value)}>
      <option value="debug">level: debug</option>
      <option value="info">level: info</option>
      <option value="warn">level: warn</option>
      <option value="error">level: error</option>
    </select>
  </div>
  <label style="margin: 0; display: flex; align-items: center; gap: 6px;">
    <input
      type="checkbox"
      bind:checked={autoScroll}
      style="width: auto;"
    /> auto-scroll
  </label>
  <div class="spacer"></div>
  <span class="faint">{shown.length} line(s)</span>
</div>

<div class="card">
  <div class="console" bind:this={consoleEl}>
    {#if shown.length === 0}
      <div class="empty">No log lines.</div>
    {:else}
      {#each shown as l (l.seq)}
        <div class="log-line">
          <span class="seq">#{l.seq}</span>
          <span class="slot" title={l.plugin}>{l.slot}</span>
          <span class="lvl {l.level}">{l.level}</span>
          <span>{l.message}</span>
        </div>
      {/each}
    {/if}
  </div>
</div>
