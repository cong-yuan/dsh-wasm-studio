<script lang="ts">
  import { onMount } from "svelte";
  import { callTool, errorMessage } from "$lib/api";
  import { getTools, refreshAll } from "$lib/state.svelte";

  // Which tool's call panel is open, and its JSON args.
  let calling = $state<string | null>(null);
  let argsText = $state("{}");
  let result = $state<string | null>(null);
  let resultErr = $state(false);
  let busy = $state(false);

  onMount(refreshAll);

  function openCall(name: string, parameters: unknown) {
    calling = name;
    argsText = sampleArgs(parameters);
    result = null;
    resultErr = false;
  }

  async function run() {
    if (!calling) return;
    busy = true;
    result = null;
    try {
      const args = argsText.trim() ? JSON.parse(argsText) : {};
      const out = await callTool(calling, args);
      result = JSON.stringify(out, null, 2);
      resultErr = false;
    } catch (e) {
      result = errorMessage(e);
      resultErr = true;
    } finally {
      busy = false;
    }
  }

  /** Build a plausible args object from a tool's JSON Schema. */
  function sampleArgs(schema: unknown): string {
    if (!schema || typeof schema !== "object") return "{}";
    const props = (schema as { properties?: Record<string, { type?: string }> })
      .properties;
    if (!props) return "{}";
    const obj: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(props)) {
      obj[k] = v?.type === "integer" || v?.type === "number" ? 0 : "";
    }
    return JSON.stringify(obj, null, 2);
  }
</script>

<div class="page-head">
  <div>
    <h1>Tools</h1>
    <div class="sub">
      Every tool a mounted plugin declares, exposed on the harness's tool
      registry. Call one directly to test it.
    </div>
  </div>
  <button onclick={() => refreshAll()}>Refresh</button>
</div>

<div class="card">
  {#if getTools().length === 0}
    <div class="empty">No tools. Load a plugin that declares some.</div>
  {:else}
    <table>
      <thead>
        <tr>
          <th>Tool</th>
          <th>Description</th>
          <th>Owner</th>
          <th>Slot</th>
          <th></th>
        </tr>
      </thead>
      <tbody>
        {#each getTools() as t}
          <tr>
            <td class="mono">{t.name}</td>
            <td class="muted">{t.description || "—"}</td>
            <td>{t.plugin}</td>
            <td class="mono muted">{t.slot}</td>
            <td style="text-align: right;">
              <button class="ghost" onclick={() => openCall(t.name, t.parameters)}>
                call
              </button>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</div>

{#if calling}
  <div
    class="overlay"
    role="presentation"
    onclick={(e) => e.target === e.currentTarget && (calling = null)}
  >
    <div class="modal">
      <h2>Call <span class="mono">{calling}</span></h2>

      <div class="field">
        <label for="args">Arguments (JSON)</label>
        <textarea id="args" rows="5" bind:value={argsText}></textarea>
      </div>

      {#if result !== null}
        <div class="muted" style="font-size: 12px; margin-bottom: 5px;">Result</div>
        <div
          class="console"
          style="padding: 12px; border: 1px solid var(--border); border-radius: var(--radius-sm);"
        >
          <pre style="margin: 0;" class:err={resultErr} style:color={resultErr ? "var(--err)" : "inherit"}>{result}</pre>
        </div>
      {/if}

      <div class="modal-actions">
        <button onclick={() => (calling = null)}>Close</button>
        <button class="primary" onclick={run} disabled={busy}>
          {busy ? "calling…" : "Call tool"}
        </button>
      </div>
    </div>
  </div>
{/if}
