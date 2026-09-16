<script lang="ts">
  import { onMount } from "svelte";
  import { ensureCapabilities, getCaps } from "$lib/state.svelte";

  onMount(ensureCapabilities);
</script>

<div class="page-head">
  <div>
    <h1>Capabilities</h1>
    <div class="sub">What a mounted WASM plugin is allowed to do.</div>
  </div>
</div>

{#if !getCaps()}
  <div class="empty">Loading…</div>
{:else}
  <div class="stat-row">
    <div class="stat">
      <div class="label">Disk compile cache</div>
      <div class="value small">
        {getCaps()?.disk_cache ? "enabled" : "disabled"}
      </div>
    </div>
    <div class="stat">
      <div class="label">Hookable flow events</div>
      <div class="value">{getCaps()?.flow_events.length ?? 0}</div>
    </div>
    <div class="stat">
      <div class="label">Inject-able dsh services</div>
      <div class="value">{getCaps()?.dsh_services.length ?? 0}</div>
    </div>
  </div>

  <div class="card" style="margin-bottom: 20px;">
    <div class="card-head">
      <h2>Flow events a plugin may hook</h2>
    </div>
    <table>
      <thead><tr><th>Event</th><th>Intervention</th></tr></thead>
      <tbody>
        {#each getCaps()?.flow_events ?? [] as ev}
          {#if ev === "tools/pre-execute" || ev === "agent/pre-step" || ev === "agent/request"}
            <tr>
              <td class="mono">{ev}</td>
              <td><span class="badge ok">rewrite / veto</span></td>
            </tr>
          {:else if ev === "assistant/chunk" || ev === "tool/result"}
            <tr>
              <td class="mono">{ev}</td>
              <td><span class="badge">rewrite</span></td>
            </tr>
          {:else}
            <tr>
              <td class="mono">{ev}</td>
              <td><span class="badge">observe</span></td>
            </tr>
          {/if}
        {/each}
      </tbody>
    </table>
  </div>

  <div class="card">
    <div class="card-head">
      <h2>dsh services a plugin may inject</h2>
    </div>
    <table>
      <thead><tr><th>Service</th></tr></thead>
      <tbody>
        {#each getCaps()?.dsh_services ?? [] as svc}
          <tr><td class="mono">{svc}</td></tr>
        {/each}
      </tbody>
    </table>
  </div>

  <div class="notice err" style="margin-top: 20px;">
    <strong>No capability/permission model yet.</strong> Plugins currently receive
    full WASI — they can read and write arbitrary files and reach the network.
    Do <strong>not</strong> run untrusted <code>.wasm</code> here.
  </div>
{/if}
