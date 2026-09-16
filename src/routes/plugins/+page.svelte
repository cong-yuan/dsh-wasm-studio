<script lang="ts">
  import { onMount } from "svelte";
  import {
    loadPlugin,
    unloadPlugin,
    reloadPlugin,
    validatePlugin,
    errorMessage,
  } from "$lib/api";
  import { getPlugins, refreshAll, setError, isLoading } from "$lib/state.svelte";

  let showLoad = $state(false);
  let busy = $state<string | null>(null);
  let notice = $state<{ kind: "ok" | "err"; text: string } | null>(null);

  // Load-form fields.
  let formSlot = $state("");
  let formPath = $state("");
  let formConfig = $state("");
  let validating = $state(false);
  let validation = $state<string | null>(null);

  // Shown as the config textarea's placeholder. Held in a variable because a
  // literal `{...}` in an attribute would be parsed as a JS expression.
  const configPlaceholder = '{ "greeting": "Hi" }';

  onMount(refreshAll);

  function flash(kind: "ok" | "err", text: string) {
    notice = { kind, text };
    setTimeout(() => (notice = null), 5000);
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
      await reloadPlugin(slot);
      flash("ok", `Reloaded “${slot}”`);
      await refreshAll();
    } catch (e) {
      // A rejected reload (broken build) leaves the old plugin running.
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
      Each <code>.wasm</code> is mounted as its own cordis plugin — own fiber,
      own <code>inject</code>/<code>provide</code>.
    </div>
  </div>
  <div class="toolbar">
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
      No plugins loaded yet. Click <strong>Load plugin</strong> and point at a
      compiled <code>.wasm</code> (e.g. <code>plugins/hello-go/hello_go.wasm</code>).
    </div>
  {:else}
    <table>
      <thead>
        <tr>
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
        <input
          id="path"
          bind:value={formPath}
          placeholder="/path/to/plugin.wasm"
        />
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
