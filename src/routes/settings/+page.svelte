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
  // Adjustments: what later-loaded plugins are doing to earlier UI. This is
  // the feature's control panel — without it, a hidden panel is a mystery.
  let adjustments = $derived.by(() => {
    void revision;
    return pluginHost.slots.listAdjustments();
  });
  // Contributions that exist but do not render because an adjustment hid them.
  let suppressed = $derived.by(() => {
    void revision;
    return pluginHost.slots.resolve().filter((r) => r.hidden);
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
                {#each contribs as c}
                  <span class="badge" class:ok={!!c.renderOwner}>
                    {c.owner}{(c.component) ? `:${c.component}` : ""}
                    {#if c.renderOwner}&nbsp;⇄ {c.renderOwner}{/if}
                  </span>
                {/each}
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

  {#if adjustments.length}
    <div class="empty" style="text-align: left; margin-top: 12px;">
      <strong>Adjustments in effect</strong>
      <span class="faint">— a later-loaded plugin reshaping existing UI.
        Releasing the plugin restores the original.</span>
      <table class="table" style="margin-top: 8px;">
        <thead>
          <tr><th>By</th><th>Target</th><th>Action</th><th>Effect</th></tr>
        </thead>
        <tbody>
          {#each adjustments as a}
            <tr>
              <td class="mono">{a.owner}</td>
              <td class="mono faint">
                {a.slot}{a.from ? ` · from ${a.from}` : ""}
              </td>
              <td><span class="badge warn">{a.action}</span></td>
              <td class="faint mono" style="font-size: 11px;">
                {#if a.action === "priority"}
                  {#if a.to !== undefined}
                    priority → {a.to}
                  {:else}
                    priority {(a.by ?? 0) >= 0 ? "+" : ""}{a.by ?? 0}
                  {/if}
                {:else if a.action === "replace"}
                  renders <b>{a.component}</b>
                {:else}
                  {a.action}
                {/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}

  {#if suppressed.length}
    <div class="empty" style="text-align: left; margin-top: 12px;">
      <strong>Suppressed</strong>
      <span class="faint">— declared but not rendering, because an adjustment
        hides it. The plugin is still loaded.</span>
      <div class="pill-list" style="margin-top: 8px;">
        {#each suppressed as r}
          <span class="badge warn">
            {r.contribution.owner} → {r.contribution.slot}
          </span>
        {/each}
      </div>
    </div>
  {/if}
</div>
