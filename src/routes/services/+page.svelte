<script lang="ts">
  import { onMount } from "svelte";
  import { getServices, getPlugins, refreshAll } from "$lib/state.svelte";

  onMount(refreshAll);

  // Build the dependency edges: slot -> the services it injects.
  let edges = $derived(
    getPlugins().flatMap((p) =>
      p.injects.map((svc) => ({ from: p.slot, to: svc })),
    ),
  );
</script>

<div class="page-head">
  <div>
    <h1>Services</h1>
    <div class="sub">
      The <code>inject</code>/<code>provide</code> graph. A service may be
      offered by a WASM slot or by the dsh harness itself.
    </div>
  </div>
  <button onclick={() => refreshAll()}>Refresh</button>
</div>

<div class="card" style="margin-bottom: 20px;">
  <div class="card-head"><h2>Available services</h2></div>
  {#if getServices().length === 0}
    <div class="empty">No services registered.</div>
  {:else}
    <table>
      <thead>
        <tr>
          <th>Service</th>
          <th>Origin</th>
          <th>Provider</th>
        </tr>
      </thead>
      <tbody>
        {#each getServices() as s}
          <tr>
            <td class="mono">{s.name}</td>
            <td>
              <span class="badge" class:ok={s.origin === "wasm"} class:dsh={s.origin === "dsh"}>
                {s.origin}
              </span>
            </td>
            <td class="mono muted">{s.provider ?? "— (host)"}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</div>

<div class="card">
  <div class="card-head"><h2>Dependencies</h2></div>
  {#if edges.length === 0}
    <div class="empty">
      No plugin declares an <code>inject</code>. A plugin with unmet
      dependencies stays <strong>pending</strong> and registers nothing.
    </div>
  {:else}
    <table>
      <thead>
        <tr><th>Plugin (slot)</th><th>requires</th><th>status</th></tr>
      </thead>
      <tbody>
        {#each edges as e}
          {@const provided = getServices().some((s) => s.name === e.to)}
          <tr>
            <td class="mono">{e.from}</td>
            <td class="mono">{e.to}</td>
            <td>
              {#if provided}
                <span class="badge ok">satisfied</span>
              {:else}
                <span class="badge warn">unmet → pending</span>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</div>
