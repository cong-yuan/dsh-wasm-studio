<script lang="ts">
  import { onMount } from "svelte";
  import {
    loadPlugin,
    unloadPlugin,
    removePlugin,
    reloadPlugin,
    validatePlugin,
    discoverPlugins,
    pluginWindows,
    openPluginWindow,
    watchStatus,
    startWatch,
    stopWatch,
    errorMessage,
    type Discovered,
    type PluginWindow,
    type SearchedRoot,
  } from "$lib/api";
  import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
  import { getCatalog, getStatus, refreshAll, isLoading } from "$lib/state.svelte";
  import { pluginHost } from "$lib/plugin-runtime";

  let showLoad = $state(false);
  let busy = $state<string | null>(null);
  let notice = $state<{ kind: "ok" | "err"; text: string } | null>(null);
  let watching = $state(false);

  // Discover modal.
  let showDiscover = $state(false);
  let discovered = $state<Discovered[]>([]);
  let searched = $state<SearchedRoot[]>([]);

  // Load-form fields.
  let formSlot = $state("");
  let formPath = $state("");
  let validating = $state(false);
  let validation = $state<string | null>(null);

  // Windows the running plugins declare.
  let windows = $state<PluginWindow[]>([]);

  async function loadWindows() {
    try {
      windows = await pluginWindows();
    } catch (e) {
      windows = [];
      void e;
    }
  }

  // Live view of which UI contributions actually render, and why not.
  let hostRevision = $state(0);
  pluginHost.subscribe(() => (hostRevision += 1));
  let diagnostics = $derived.by(() => {
    void hostRevision;
    return pluginHost.diagnostics();
  });

  onMount(async () => {
    await refreshAll();
    await loadWindows();
    watching = await watchStatus().catch(() => false);
  });

  /** Refresh both the plugins and the window list. */
  async function refreshBoth() {
    await refreshAll();
    await loadWindows();
  }

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
      const result = await discoverPlugins();
      discovered = result.plugins;
      searched = result.searched;
      showDiscover = true;
    } catch (e) {
      flash("err", errorMessage(e));
    }
  }

  /** Open the app's plugins directory in the OS file manager. */
  async function revealPluginsDir() {
    const dir = getStatus()?.plugins_dir;
    if (!dir) return;
    try {
      const { revealItemInDir } = await import("@tauri-apps/plugin-opener");
      await revealItemInDir(dir);
    } catch (e) {
      flash("err", errorMessage(e));
    }
  }

  /** Native file picker — removes the "what do I type?" problem entirely. */
  async function pickFile() {
    try {
      const picked = await openFileDialog({
        multiple: false,
        filters: [{ name: "WebAssembly", extensions: ["wasm"] }],
      });
      if (typeof picked === "string") {
        formPath = picked;
        if (!formSlot.trim()) {
          const base = picked.split("/").pop() ?? "plugin";
          formSlot = base.replace(/\.wasm$/, "").replace(/-/g, "_");
        }
        validation = null;
      }
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

  async function doLoad(slot?: string, path?: string) {
    const s = (slot ?? formSlot).trim();
    const p = (path ?? formPath).trim();
    if (!s || !p) return;
    busy = s;
    try {
      await loadPlugin(s, p);
      flash("ok", `Started “${s}”`);
      showLoad = false;
      formSlot = formPath = "";
      validation = null;
      await refreshBoth();
    } catch (e) {
      flash("err", errorMessage(e));
    } finally {
      busy = null;
    }
  }

  async function doStop(slot: string) {
    busy = slot;
    try {
      await unloadPlugin(slot);
      flash("ok", `Stopped “${slot}” (still listed — restart any time)`);
      await refreshBoth();
    } catch (e) {
      flash("err", errorMessage(e));
    } finally {
      busy = null;
    }
  }

  async function doRemove(slot: string) {
    if (!confirm(`Remove “${slot}” from the list? Its .wasm file is not touched.`)) return;
    busy = slot;
    try {
      await removePlugin(slot);
      flash("ok", `Removed “${slot}” from the list`);
      await refreshBoth();
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
      await refreshBoth();
    } catch (e) {
      flash("err", `Reload rejected — ${errorMessage(e)}`);
    } finally {
      busy = null;
    }
  }
</script>

<div class="page-head">
  <div>
    <h1>Plugins</h1>
    <div class="sub">
      Every plugin the app knows about. <span style="color: var(--ok)">Green</span>
      means running; white means stopped or not yet started.
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
    <button onclick={() => refreshBoth()} disabled={isLoading()}>Refresh</button>
  </div>
</div>

{#if notice}
  <div class="notice" class:ok={notice.kind === "ok"} class:err={notice.kind === "err"}>
    {notice.text}
  </div>
{/if}

<div class="card">
  {#if getCatalog().length === 0}
    <div class="empty">
      No plugins found. Use <strong>Discover…</strong> to see where the app
      looks, or drop a <code>.wasm</code> into the plugins directory.
    </div>
  {:else}
    <table>
      <thead>
        <tr>
          <th>Slot</th>
          <th>Plugin</th>
          <th>Status</th>
          <th>Tools</th>
          <th>Source</th>
          <th></th>
        </tr>
      </thead>
      <tbody>
        {#each getCatalog() as p}
          <tr>
            <!-- The slot name is the row's identity; colour encodes running. -->
            <td class="mono" class:running={p.running}>{p.slot}</td>
            <td class:running={p.running}>
              {p.plugin}
              {#if !p.exists}
                <span class="badge err" title="the .wasm file is missing">missing file</span>
              {/if}
            </td>
            <td>
              {#if p.running && p.active}
                <span class="badge ok">running</span>
              {:else if p.running}
                <span class="badge warn">{p.state}</span>
              {:else}
                <span class="badge">stopped</span>
              {/if}
            </td>
            <td>{p.running ? p.tool_count : "—"}</td>
            <td class="faint" style="font-size: 11px;">
              {#if p.in_config}configured{:else}found on disk{/if}
            </td>
            <td>
              <div class="toolbar" style="justify-content: flex-end;">
                {#if p.running}
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
                    onclick={() => doStop(p.slot)}
                    disabled={busy === p.slot}
                    title="Stop the plugin (it stays in the list)"
                  >
                    stop
                  </button>
                  <button
                    class="ghost"
                    onclick={() => doRemove(p.slot)}
                    disabled={busy === p.slot}
                    title="Forget it (delete from the list; the file is untouched)"
                  >
                    remove
                  </button>
                {:else}
                  <button
                    class="ghost"
                    onclick={() => doLoad(p.slot, p.path)}
                    disabled={busy === p.slot || !p.exists}
                    title="Start this plugin"
                  >
                    {busy === p.slot ? "starting…" : "start"}
                  </button>
                {/if}
              </div>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</div>

<div class="card" style="margin-top: 20px;">
  <div class="card-head">
    <h2>Plugin windows</h2>
    <span class="faint" style="font-size: 11px;">
      {windows.length} declared
    </span>
  </div>
  {#if windows.length === 0}
    <div class="empty">
      No running plugin declares a window. A plugin opts in by adding a
      <code>windows</code> array to its <code>ui</code> block.
    </div>
  {:else}
    <table>
      <thead>
        <tr><th>Window</th><th>Plugin</th><th>Renders</th><th>Opens</th><th></th></tr>
      </thead>
      <tbody>
        {#each windows as w}
          <tr>
            <td>
              {w.title}
              <div class="mono faint" style="font-size: 10px;">{w.label}</div>
            </td>
            <td class="mono">{w.slot}</td>
            <td>
              {#if w.content === "html"}
                <span class="badge">standalone html</span>
              {:else}
                <span class="badge">component</span>
                <span class="mono faint" style="font-size: 11px;">{w.component}</span>
              {/if}
            </td>
            <td>
              {#if w.open === "startup"}
                <span class="badge ok">startup</span>
              {:else if w.open === "auto"}
                <span class="badge ok">auto</span>
              {:else}
                <span class="badge">manual</span>
              {/if}
            </td>
            <td style="text-align: right;">
              <button
                class="ghost"
                onclick={async () => {
                  try {
                    await openPluginWindow(w.label);
                  } catch (e) {
                    flash("err", errorMessage(e));
                  }
                }}
              >
                open
              </button>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</div>

<div class="card" style="margin-top: 20px;">
  <div class="card-head">
    <h2>Frontend UI</h2>
    <span class="faint" style="font-size: 11px;">
      {diagnostics.length} contribution(s)
    </span>
  </div>
  {#if diagnostics.length === 0}
    <div class="empty">
      No running plugin declares frontend UI. A plugin opts in by adding a
      <code>ui</code> block to its declaration (see <code>docs/ABI.md</code>).
    </div>
  {:else}
    <table>
      <thead>
        <tr><th>Plugin</th><th>Target slot</th><th>Component</th><th>Status</th></tr>
      </thead>
      <tbody>
        {#each diagnostics as d}
          <tr>
            <td class="mono">{d.owner}</td>
            <td class="mono">{d.slot}</td>
            <td class="mono faint">{d.component ?? "—"}</td>
            <td>
              {#if d.status === "ready"}
                <span class="badge ok">ready</span>
              {:else}
                <span class="badge warn">{d.status}</span>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</div>

{#if showLoad}
  <div
    class="overlay"
    role="presentation"
    onclick={(e) => e.target === e.currentTarget && (showLoad = false)}
  >
    <div class="modal">
      <h2>Start a WASM plugin</h2>

      <div class="field">
        <label for="slot">Slot (stable identity, survives reload)</label>
        <input id="slot" bind:value={formSlot} placeholder="greet" />
      </div>

      <div class="field">
        <label for="path">Path to .wasm</label>
        <div class="toolbar">
          <input
            id="path"
            bind:value={formPath}
            placeholder="pick a file, or type a path"
            style="flex:1"
          />
          <button type="button" onclick={pickFile}>Browse…</button>
        </div>
        <div class="faint" style="font-size: 11px; margin-top: 4px;">
          Tip: <strong>Discover…</strong> lists every <code>.wasm</code> already on disk.
        </div>
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
          onclick={() => doLoad()}
          disabled={busy !== null || !formSlot.trim() || !formPath.trim()}
        >
          {busy ? "starting…" : "Start"}
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
        <div class="empty" style="text-align: left;">
          <p style="margin-top: 0;">
            No unconfigured <code>.wasm</code> files found. Directories searched:
          </p>
          <table>
            <thead><tr><th>Directory</th><th>Kind</th><th>Exists</th></tr></thead>
            <tbody>
              {#each searched as r}
                <tr>
                  <td class="mono" style="font-size: 11px;">{r.path}</td>
                  <td class="faint">{r.label}</td>
                  <td>
                    {#if r.exists}<span class="badge ok">yes</span>{:else}<span class="badge err">no</span>{/if}
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
          <p class="muted" style="margin-bottom: 0;">
            Drop a <code>.wasm</code> into the <strong>app plugin directory</strong>
            above, or use <strong>Start a plugin → Browse…</strong>.
          </p>
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
                    onclick={() => doLoad(d.slot, d.path)}
                    disabled={busy !== null}
                  >
                    start
                  </button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
        <details style="margin-top: 12px;">
          <summary class="faint" style="font-size: 11px; cursor: pointer;">
            searched {searched.length} location(s)
          </summary>
          <div style="margin-top: 8px;">
            {#each searched as r}
              <div class="faint mono" style="font-size: 11px;">
                {r.path} <span style="opacity:.6">— {r.wasm_count} new</span>
              </div>
            {/each}
          </div>
        </details>
      {/if}
      <div class="modal-actions">
        <button onclick={revealPluginsDir}>Open plugins folder</button>
        <button onclick={() => (showDiscover = false)}>Close</button>
      </div>
    </div>
  </div>
{/if}

<style>
  /* Running plugins are green; stopped ones keep the normal text colour. */
  .running {
    color: var(--ok);
  }
</style>
