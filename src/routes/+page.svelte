<script lang="ts">
  import { onMount } from "svelte";
  import {
    getStatus,
    getPlugins,
    getTools,
    getServices,
    getLastError,
    getEvents,
    isLoading,
    refreshAll,
  } from "$lib/state.svelte";

  onMount(refreshAll);
</script>

<div class="page-head">
  <div>
    <h1>Overview</h1>
    <div class="sub">A dsh-rs agent harness with WASM plugins mounted as cordis plugins.</div>
  </div>
  <button onclick={refreshAll} disabled={isLoading()}>
    {isLoading() ? "refreshing…" : "Refresh"}
  </button>
</div>

{#if getLastError()}
  <div class="notice err">Backend error: {getLastError()}</div>
{/if}

<div class="stat-row">
  <div class="stat">
    <div class="label">Plugins</div>
    <div class="value">{getPlugins().length}</div>
  </div>
  <div class="stat">
    <div class="label">Active</div>
    <div class="value">{getPlugins().filter((p) => p.active).length}</div>
  </div>
  <div class="stat">
    <div class="label">WASM tools</div>
    <div class="value">{getTools().length}</div>
  </div>
  <div class="stat">
    <div class="label">Tools (incl. dsh)</div>
    <div class="value">{getStatus()?.tool_count ?? "—"}</div>
  </div>
  <div class="stat">
    <div class="label">Services</div>
    <div class="value">{getServices().length}</div>
  </div>
</div>

<div class="stat-row">
  <div class="stat">
    <div class="label">Plugins directory</div>
    <div class="value small">{getStatus()?.plugins_dir ?? "—"}</div>
  </div>
  <div class="stat">
    <div class="label">Auto-reload</div>
    <div class="value small">
      {getStatus()?.watching ? "watching .wasm for rebuilds" : "off"}
    </div>
  </div>
</div>

{#if getEvents().length}
  <div class="card" style="margin-bottom: 20px;">
    <div class="card-head"><h2>Recent activity</h2></div>
    <div style="padding: 8px 0;">
      {#each getEvents().slice(0, 6) as ev}
        <div class="log-line" style="grid-template-columns: 1fr;">
          {#if ev.kind === "reloaded"}
            <span><span class="badge ok">reloaded</span> <span class="mono">{ev.slot}</span> — tools: {ev.tools.join(", ") || "none"}</span>
          {:else if ev.kind === "reload_failed"}
            <span><span class="badge err">reload failed</span> <span class="mono">{ev.slot}</span> — {ev.error}</span>
          {:else}
            <span><span class="badge">changed</span></span>
          {/if}
        </div>
      {/each}
    </div>
  </div>
{/if}

<div class="card">
  <div class="card-head">
    <h2>Mounted plugins</h2>
    <a href="/plugins" class="faint" style="font-size: 12px;">manage →</a>
  </div>
  {#if getPlugins().length === 0}
    <div class="empty">
      No plugins loaded. Go to <a href="/plugins">Plugins</a> to attach a <code>.wasm</code>.
    </div>
  {:else}
    <table>
      <thead>
        <tr>
          <th>Slot</th>
          <th>Plugin</th>
          <th>State</th>
          <th>Tools</th>
          <th>Provides</th>
        </tr>
      </thead>
      <tbody>
        {#each getPlugins() as p}
          <tr>
            <td class="mono">{p.slot}</td>
            <td>{p.plugin}</td>
            <td>
              <span class="badge" class:ok={p.active} class:warn={!p.active}>
                {p.state}
              </span>
            </td>
            <td>{p.tool_count}</td>
            <td>
              {#if p.provides.length}
                <div class="pill-list">
                  {#each p.provides as s}<span class="badge">{s}</span>{/each}
                </div>
              {:else}
                <span class="faint">—</span>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</div>
