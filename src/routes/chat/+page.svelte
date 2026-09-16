<script lang="ts">
  import { onMount, tick } from "svelte";
  import {
    createAgent,
    listAgents,
    sendMessage,
    steerAgent,
    cancelAgent,
    disposeAgent,
    transcript,
    errorMessage,
    type AgentRow,
    type ChatMessage,
  } from "$lib/api";
  import { getStatus, getTools } from "$lib/state.svelte";
  import Slot from "$lib/Slot.svelte";

  let agents = $state<AgentRow[]>([]);
  let activeId = $state<string | null>(null);
  let messages = $state<ChatMessage[]>([]);
  let input = $state("");
  let sending = $state(false);
  let err = $state<string | null>(null);
  let transcriptEl = $state<HTMLElement | null>(null);

  // New-agent form.
  let showNew = $state(false);
  let provider = $state("mock");
  let model = $state("mock-1");
  let cwd = $state("/tmp");
  let agentId = $state("");

  // Monotonic message ids (the backend requires one per message).
  let seq = 0;
  const nextId = () => `ui-${++seq}`;

  onMount(async () => {
    await refreshAgents();
    if (agents.length > 0) await select(agents[0].id);
  });

  async function refreshAgents() {
    try {
      agents = await listAgents();
      err = null;
    } catch (e) {
      err = errorMessage(e);
    }
  }

  async function select(id: string) {
    activeId = id;
    try {
      messages = await transcript(id);
      scrollDown();
    } catch (e) {
      err = errorMessage(e);
    }
  }

  async function doCreate() {
    try {
      const id = agentId.trim()
        ? await createAgent(provider, model, cwd, agentId.trim())
        : await createAgent(provider, model, cwd);
      showNew = false;
      agentId = "";
      await refreshAgents();
      await select(id);
    } catch (e) {
      err = errorMessage(e);
    }
  }

  async function submit() {
    if (!input.trim() || !activeId) return;
    const text = input;
    input = "";
    sending = true;
    err = null;
    try {
      // Send and wait for the turn; the backend resolves when the agent is idle.
      await sendMessage(activeId, text, nextId());
      messages = await transcript(activeId);
      await refreshAgents();
      scrollDown();
    } catch (e) {
      err = errorMessage(e);
    } finally {
      sending = false;
    }
  }

  async function doCancel() {
    if (!activeId) return;
    try {
      await cancelAgent(activeId);
      await refreshAgents();
    } catch (e) {
      err = errorMessage(e);
    }
  }

  async function doDispose(id: string) {
    try {
      await disposeAgent(id);
      if (activeId === id) {
        activeId = null;
        messages = [];
      }
      await refreshAgents();
    } catch (e) {
      err = errorMessage(e);
    }
  }

  async function scrollDown() {
    await tick();
    if (transcriptEl) transcriptEl.scrollTop = transcriptEl.scrollHeight;
  }

  let activeAgent = $derived(agents.find((a) => a.id === activeId) ?? null);
</script>

<div class="page-head">
  <div>
    <h1>Chat</h1>
    <div class="sub">
      Drive the agent loop. Tool calls are dispatched to mounted WASM plugins —
      {getTools().length} tool(s) available.
    </div>
  </div>
  <div class="toolbar">
    <!-- plugins may add actions here -->
    <Slot slot="agent.actions" />
    {#if activeAgent?.busy}
      <span class="badge warn">running</span>
      <button class="ghost danger" onclick={doCancel}>cancel</button>
    {/if}
    <button onclick={() => refreshAgents()}>Refresh</button>
    <button class="primary" onclick={() => (showNew = true)}>New agent</button>
  </div>
</div>

{#if err}
  <div class="notice err">{err}</div>
{/if}

{#if !getStatus()?.booted}
  <div class="notice err">The harness is not booted; chat is unavailable.</div>
{/if}

<div class="chat-shell">
  <aside class="chat-agents">
    {#if agents.length === 0}
      <div class="faint" style="padding: 12px; font-size: 12px;">
        No agents. Create one to start.
      </div>
    {:else}
      {#each agents as a}
        <div class="agent-row" class:active={a.id === activeId}>
          <button class="agent-pick" onclick={() => select(a.id)}>
            <span class="mono">{a.id}</span>
            <span class="faint" style="font-size: 11px;">{a.messages} msg · {a.turns} ev</span>
          </button>
          <button class="ghost danger" onclick={() => doDispose(a.id)} title="Dispose">×</button>
        </div>
      {/each}
    {/if}
  </aside>

  <section class="chat-main">
    {#if !activeId}
      <div class="empty">Select or create an agent to begin.</div>
    {:else}
      <div class="transcript" bind:this={transcriptEl}>
        {#if messages.length === 0}
          <div class="empty">No messages yet — say something.</div>
        {/if}
        {#each messages as m}
          <div class="msg {m.role}">
            <div class="msg-role">{m.role}</div>
            <div class="msg-body">
              {#if m.reasoning}
                <details class="reasoning">
                  <summary>reasoning</summary>
                  <pre>{m.reasoning}</pre>
                </details>
              {/if}
              {#if m.text}
                <div class="msg-text">{m.text}</div>
              {/if}
              {#each m.tool_calls as c}
                <div class="tool-call">
                  <span class="badge">→ {c.name}</span>
                  <code>{c.arguments}</code>
                </div>
              {/each}
              {#each m.tool_results as r}
                <div class="tool-result" class:err={r.is_error}>
                  <span class="badge" class:err={r.is_error} class:ok={!r.is_error}>
                    {r.is_error ? "error" : "result"}
                  </span>
                  <pre>{r.content}</pre>
                </div>
              {/each}
            </div>
          </div>
        {/each}
      </div>

      <div class="composer">
        <textarea
          bind:value={input}
          rows="2"
          placeholder="Message the agent… (Enter to send, Shift+Enter for newline)"
          onkeydown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              submit();
            }
          }}
        ></textarea>
        <button class="primary" onclick={submit} disabled={sending || !input.trim()}>
          {sending ? "…" : "Send"}
        </button>
      </div>
    {/if}
  </section>
</div>

{#if showNew}
  <div
    class="overlay"
    role="presentation"
    onclick={(e) => e.target === e.currentTarget && (showNew = false)}
  >
    <div class="modal">
      <h2>New agent</h2>

      <div class="field">
        <label for="provider">Provider route</label>
        <input id="provider" bind:value={provider} placeholder="mock" />
        <div class="faint" style="font-size: 11px; margin-top: 4px;">
          <code>mock</code> always works (echoes input) and is enough to exercise
          tool calls. Configure real providers under <code>extra.llm</code> in
          studio.json.
        </div>
      </div>

      <div class="field">
        <label for="model">Model</label>
        <input id="model" bind:value={model} placeholder="mock-1" />
      </div>

      <div class="field">
        <label for="cwd">Working directory (for built-in tools)</label>
        <input id="cwd" bind:value={cwd} placeholder="/tmp" />
      </div>

      <div class="field">
        <label for="aid">Agent id (optional)</label>
        <input id="aid" bind:value={agentId} placeholder="auto" />
      </div>

      <div class="modal-actions">
        <button onclick={() => (showNew = false)}>Cancel</button>
        <button class="primary" onclick={doCreate}>Create</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .chat-shell {
    display: grid;
    grid-template-columns: 200px 1fr;
    gap: 16px;
    height: calc(100vh - 160px);
  }
  .chat-agents {
    background: var(--bg-panel);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    overflow-y: auto;
    padding: 6px;
  }
  .agent-row {
    display: flex;
    align-items: center;
    gap: 4px;
    border-radius: var(--radius-sm);
  }
  .agent-row.active {
    background: var(--accent-soft);
  }
  .agent-pick {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 2px;
    background: none;
    border: none;
    padding: 8px 10px;
    text-align: left;
    cursor: pointer;
  }
  .chat-main {
    display: flex;
    flex-direction: column;
    background: var(--bg-panel);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    overflow: hidden;
  }
  .transcript {
    flex: 1;
    overflow-y: auto;
    padding: 16px;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  .msg {
    display: grid;
    grid-template-columns: 72px 1fr;
    gap: 10px;
  }
  .msg-role {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-muted);
    padding-top: 3px;
  }
  .msg.user .msg-role {
    color: var(--accent);
  }
  .msg-body {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .msg-text {
    white-space: pre-wrap;
    word-break: break-word;
  }
  .reasoning summary {
    cursor: pointer;
    color: var(--text-faint);
    font-size: 12px;
  }
  .reasoning pre {
    margin: 6px 0 0;
    padding: 8px;
    background: var(--bg-input);
    border-radius: var(--radius-sm);
    color: var(--text-muted);
    white-space: pre-wrap;
    font-size: 11px;
  }
  .tool-call,
  .tool-result {
    display: flex;
    align-items: flex-start;
    gap: 8px;
    font-size: 12px;
  }
  .tool-call code {
    color: var(--text-muted);
    word-break: break-all;
  }
  .tool-result pre {
    margin: 0;
    padding: 6px 8px;
    background: var(--bg-input);
    border-radius: var(--radius-sm);
    white-space: pre-wrap;
    word-break: break-word;
    flex: 1;
  }
  .tool-result.err pre {
    color: var(--err);
  }
  .composer {
    display: flex;
    gap: 8px;
    padding: 12px;
    border-top: 1px solid var(--border);
    align-items: flex-end;
  }
  .composer textarea {
    resize: none;
  }
</style>
