<script lang="ts">
  import { onMount } from "svelte";
  import {
    loadPlugin,
    unloadPlugin,
    reloadPlugin,
    setPluginEnabled,
    validatePlugin,
    discoverPlugins,
    watchStatus,
    startWatch,
    stopWatch,
    errorMessage,
    type Discovered,
  } from "$lib/api";
  import { getPlugins, refreshAll, isLoading } from "$lib/state.svelte";
  import Slot from "$lib/Slot.svelte";

  let showLoad = $state(false);
  let busy = $state<string | null>(null);
  let notice = $state<{ kind: "ok" | "err"; text: string } | null>(null);
  let watching = $state(false);

  // Discover modal.
  let showDiscover = $state(false);
  let discovered = $state<Discovered[]>([]);

  // Load-form fields.
  let formSlot = $state("");
  let formPath = $state("");
  let formConfig = $state("");
  let validating = $state(false);
  let validation = $state<string | null>(null);

  const configPlaceholder = '{ "greeting": "Hi" }';

  onMount(async () => {
    await refreshAll();
    watching = await watchStatus().catch(() => false);
  });

  function flash(kind: "ok" | "err", text: string) {
    notice = { kind, text };
    setTimeout(() => (notice = null), 6000);
  }

  async function toggleWatch() {
    try {
      if (watching) await stopWatch();
      else await startWatch();
      watching = await watchStatus();
    } catch (e) {
      flash("err", errorMessage(e));
    }
  }

  async function doDiscover() {
    try {
      discovered = await discoverPlugins();
      showDiscover = true;
    } catch (e) {
      flash("err", errorMessage(e));
    }
  }

  async function doValidate() {
    validating = true;
    validation = null;
    try {
      const [name, tools] = await validatePlugin(formPath.trim());
      validation = `✓ valid — plugin “${name}”, ${tools.length} tool(s): ${tools.join(", ") || "none"}`;
    } catch (e) {
      validation = null;
      flash("err", `Invalid: ${errorMessage(e)}`);
    } finally {
      validating = false;
    }
  }

  async function doLoad() {
    busy = "load";
    try {
      const cfg = formConfig.trim() ? JSON.parse(formConfig) : null;
      await loadPlugin(formSlot.trim(), formPath.trim(), cfg);
      flash("ok", `Loaded slot “${formSlot}”`);
      showLoad = false;
      formSlot = formPath = formConfig = "";
      validation = null;
      await refreshAll();
    } catch (e) {
      flash("err", errorMessage(e));
    } finally {
      busy = null;
    }
  }

  async function doUnload(slot: string) {
    busy = slot;
    try {
      await unloadPlugin(slot);
      flash("ok", `Unloaded “${slot}”`);
      await refreshAll();
    } catch (e) {
      flash("err", errorMessage(e));
    } finally {
      busy = null;
    }
  }

  async function doReload(slot: string) {
    busy = slot;
    try {
      const tools = await reloadPlugin(slot);
      flash("ok", `Reloaded “${slot}” — tools: ${tools.join(", ") || "none"}`);
      await refreshAll();
    } catch (e) {
      // A rejected reload (broken build) leaves the old plugin running.
      flash("err", `Reload rejected — ${errorMessage(e)}`);
    } finally {
      busy = null;
    }
  }

  async function doToggle(slot: string, enabled: boolean) {
    busy = slot;
    try {
      await setPluginEnabled(slot, enabled);
      flash("ok", `${enabled ? "Enabled" : "Disabled"} “${slot}”`);
      await refreshAll();
    } catch (e) {
      flash("err", errorMessage(e));
    } finally {
      busy = null;
    }
  }
</script>

<div class="page-head">
  <div>
    <h1>Plugins</h1>
    <div class="sub">
      Each <code>.wasm</code> is mounted as its own cordis plugin — own fiber,
      own <code>inject</code>/<code>provide</code>. State persists across restarts.
    </div>
  </div>
  <div class="toolbar">
    <button
      class="ghost"
      onclick={toggleWatch}
      title="Watch each plugin's .wasm and hot-reload on rebuild"
    >
      {watching ? "◉ auto-reload on" : "○ auto-reload off"}
    </button>
    <button onclick={doDiscover}>Discover…</button>
    <button onclick={() => refreshAll()} disabled={isLoading()}>Refresh</button>
    <button class="primary" onclick={() => (showLoad = true)}>Load plugin</button>
  </div>
</div>

{#if notice}
  <div class="notice" class:ok={notice.kind === "ok"} class:err={notice.kind === "err"}>
    {notice.text}
  </div>
{/if}

<div class="card">
  {#if getPlugins().length === 0}
    <div class="empty">
      No plugins loaded. Click <strong>Discover</strong> to scan the plugins
      directory, or <strong>Load plugin</strong> to point at a <code>.wasm</code>.
    </div>
  {:else}
    <table>
      <thead>
        <tr>
          <th>On</th>
          <th>Slot</th>
          <th>Plugin</th>
          <th>State</th>
          <th>Tools</th>
          <th>Injects</th>
          <th>Provides</th>
          <th></th>
        </tr>
      </thead>
      <tbody>
        {#each getPlugins() as p}
          <tr>
            <td>
              <input
                type="checkbox"
                style="width: auto;"
                checked={p.enabled}
                disabled={busy === p.slot}
                onchange={(e) => doToggle(p.slot, e.currentTarget.checked)}
                title="Enable/disable and persist"
              />
            </td>
            <td class="mono">{p.slot}</td>
            <td>{p.plugin}</td>
            <td>
              <span class="badge" class:ok={p.active} class:warn={!p.active}>
                {p.state}
              </span>
              {#if !p.mounted}<span class="faint" style="font-size: 11px;"> unmounted</span>{/if}
            </td>
            <td>{p.tool_count}</td>
            <td>
              {#if p.injects.length}
                <div class="pill-list">
                  {#each p.injects as s}<span class="badge">{s}</span>{/each}
                </div>
              {:else}<span class="faint">—</span>{/if}
            </td>
            <td>
              {#if p.provides.length}
                <div class="pill-list">
                  {#each p.provides as s}<span class="badge">{s}</span>{/each}
                </div>
              {:else}<span class="faint">—</span>{/if}
            </td>
            <td>
              <div class="toolbar" style="justify-content: flex-end;">
                <button
                  class="ghost"
                  onclick={() => doReload(p.slot)}
                  disabled={busy === p.slot}
                  title="Atomically swap in a rebuilt .wasm"
                >
                  reload
                </button>
                <button
                  class="ghost danger"
                  onclick={() => doUnload(p.slot)}
                  disabled={busy === p.slot}
                  title="Unload and release the wasm instance"
                >
                  unload
                </button>
              </div>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</div>

<div class="card" style="margin-top: 20px;">
  <div class="card-head"><h2>Plugin detail</h2></div>
  <div style="padding: 12px 16px;">
    <!-- plugins render their own detail panels here -->
    <Slot slot="plugin.detail" empty={true} />
  </div>
</div>

{#if showLoad}
  <div
    class="overlay"
    role="presentation"
    onclick={(e) => e.target === e.currentTarget && (showLoad = false)}
  >
    <div class="modal">
      <h2>Load a WASM plugin</h2>

      <div class="field">
        <label for="slot">Slot (stable identity, survives reload)</label>
        <input id="slot" bind:value={formSlot} placeholder="greet" />
      </div>

      <div class="field">
        <label for="path">Path to .wasm</label>
        <input id="path" bind:value={formPath} placeholder="/path/to/plugin.wasm" />
      </div>

      <div class="field">
        <label for="config">Config JSON (optional, injected into the guest)</label>
        <textarea id="config" rows="3" bind:value={formConfig} placeholder={configPlaceholder}></textarea>
      </div>

      {#if validation}
        <div class="notice ok">{validation}</div>
      {/if}

      <div class="modal-actions">
        <button onclick={doValidate} disabled={validating || !formPath.trim()}>
          {validating ? "validating…" : "Validate"}
        </button>
        <div class="spacer"></div>
        <button onclick={() => (showLoad = false)}>Cancel</button>
        <button
          class="primary"
          onclick={doLoad}
          disabled={busy === "load" || !formSlot.trim() || !formPath.trim()}
        >
          {busy === "load" ? "loading…" : "Load"}
        </button>
      </div>
    </div>
  </div>
{/if}

{#if showDiscover}
  <div
    class="overlay"
    role="presentation"
    onclick={(e) => e.target === e.currentTarget && (showDiscover = false)}
  >
    <div class="modal">
      <h2>Discovered plugins</h2>
      {#if discovered.length === 0}
        <div class="empty">
          No new <code>.wasm</code> files in the plugins directory. Drop one in
          and click Discover again.
        </div>
      {:else}
        <table>
          <thead><tr><th>Slot</th><th>Path</th><th></th></tr></thead>
          <tbody>
            {#each discovered as d}
              <tr>
                <td class="mono">{d.slot}</td>
                <td class="mono muted" style="font-size: 11px;">{d.path}</td>
                <td style="text-align: right;">
                  <button
                    class="ghost"
                    onclick={async () => {
                      busy = "load";
                      try {
                        await loadPlugin(d.slot, d.path);
                        discovered = discovered.filter((x) => x.slot !== d.slot);
                        flash("ok", `Loaded “${d.slot}”`);
                        await refreshAll();
                      } catch (e) {
                        flash("err", errorMessage(e));
                      } finally {
                        busy = null;
                      }
                    }}
                    disabled={busy === "load"}
                  >
                    load
                  </button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
      <div class="modal-actions">
        <button onclick={() => (showDiscover = false)}>Close</button>
      </div>
    </div>
  </div>
{/if}
