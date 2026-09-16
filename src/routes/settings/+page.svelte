<script lang="ts">
  /**
   * Settings. The app itself has only a couple of built-in controls; the point
   * of this page is the `settings.tabs` slot — plugins contribute their own
   * settings panels here, and one plugin may even open a *sub-slot* that other
   * plugins fill (the "later plugin uses an earlier plugin's slot" case).
   */
  import Slot from "$lib/Slot.svelte";
  import { pluginHost } from "$lib/plugin-runtime";
  import { getStatus } from "$lib/state.svelte";

  // A revision so the slot list re-reads when plugins change.
  let revision = $state(0);
  pluginHost.subscribe(() => (revision += 1));
  let slots = $derived.by(() => {
    void revision;
    return pluginHost.slots.listSlots();
  });
  let pending = $derived.by(() => {
    void revision;
    return pluginHost.slots.pending();
  });
</script>

<div class="page-head">
  <div>
    <h1>Settings</h1>
    <div class="sub">
      Plugin-contributed panels, plus the slot graph they build.
    </div>
  </div>
</div>

<div class="stat-row">
  <div class="stat">
    <div class="label">Plugins directory</div>
    <div class="value small">{getStatus()?.plugins_dir ?? "—"}</div>
  </div>
  <div class="stat">
    <div class="label">Config file</div>
    <div class="value small">{getStatus()?.config_path ?? "—"}</div>
  </div>
</div>

<div class="card" style="margin-bottom: 20px;">
  <div class="card-head"><h2>Plugin settings</h2></div>
  <div style="padding: 16px;">
    <Slot slot="settings.tabs" empty={true} />
  </div>
</div>

<div class="card">
  <div class="card-head"><h2>Slot graph</h2></div>
  <table>
    <thead>
      <tr><th>Slot</th><th>Opened by</th><th>Contributors</th></tr>
    </thead>
    <tbody>
      {#each slots as s}
        {@const contribs = pluginHost.contributionsFor(s.name)}
        <tr>
          <td class="mono">{s.name}</td>
          <td>
            <span class="badge" class:ok={s.owner !== "app"}>{s.owner}</span>
          </td>
          <td>
            {#if contribs.length}
              <div class="pill-list">
                {#each contribs as c}<span class="badge">{c.owner}{(c.component) ? `:${c.component}` : ""}</span>{/each}
              </div>
            {:else}
              <span class="faint">—</span>
            {/if}
          </td>
        </tr>
      {/each}
    </tbody>
  </table>
  {#if pending.length}
    <div class="empty" style="text-align: left;">
      <strong>Pending contributions</strong> (waiting for their slot):
      <div class="pill-list" style="margin-top: 8px;">
        {#each pending as p}
          <span class="badge warn">{p.owner} → {p.slot}</span>
        {/each}
      </div>
    </div>
  {/if}
</div>
