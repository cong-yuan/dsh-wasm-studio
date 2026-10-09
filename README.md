# dsh-wasm-studio

A **Tauri desktop app** that composes a [`dsh-rs`](https://crates.io/crates/dsh-rs)
agent harness with the [`wasm-plugin-host`](../wasm-plugin-host) WASM plugin
runtime, and puts a Supabase-style admin panel in front of it.

```
┌──────────────────────────────────────────────────────────────┐
│  Svelte 5 (SvelteKit, adapter-static) — admin panel          │
│  Chat · Overview · Plugins · Tools · Services · Logs · Capabilities │
└───────────────────────────┬──────────────────────────────────┘
                            │ Tauri IPC (#[tauri::command])
┌───────────────────────────▼──────────────────────────────────┐
│  Studio (Rust, managed state)                                │
│   ├─ cordis::Context  ← dsh-rs base bundle (agent loop,      │
│   │                     sessions, tools, llm seam)           │
│   └─ WasmHost         ← wasm-plugin-host runtime             │
│         each slot mounted as its OWN cordis plugin:          │
│           own fiber · inject · provide (WasmService)         │
└──────────────────────────────────────────────────────────────┘
```

## The model

A `.wasm` file **is a plugin**, and it is a first-class dsh participant:

| Plugin declares | Effect in the harness |
|---|---|
| `tools` | registered on `ctx.tools`; the agent loop calls them like built-ins |
| `hooks` | attached to dsh's flow points — at `tools/pre-execute` it can rewrite or **veto** a real tool call |
| `injects: ["x"]` | becomes a cordis `Injection`; while `x` is missing the slot stays **PENDING** |
| `provides: ["y"]` | published as `ctx.provide("y", WasmService)`; any dsh plugin can `require` and call it |
| unload | the wasmtime instance is dropped — code **and linear memory** really released |

Dependencies may be satisfied by another WASM slot's `provides` **or** by a dsh
service (`WasmHost::declare_dsh_service`).

## A plugin can own the launch view

A plugin may declare a window with `"open": "startup"`, and the app then opens
**that** window at launch instead of its own page. The app's own window is
created hidden, so the default page never flashes first; if boot leaves nothing
visible (the startup window failed to open) the app window is revealed rather
than leaving a headless process.

Exactly one plugin may claim it — two claimants is an **error**, not a
coin-flip, because "which window starts" must have one answer.

This is how the **HanaAgent shell** ships: `plugins/hana-shell` in the
`wasm-plugin-host` repo declares `open: "startup"` and takes the window. It is a
WASM plugin like any other — there is no special case in this app for it.

## Layout

```
src/                  SvelteKit frontend
  lib/api.ts          typed wrappers over every Tauri command
  lib/state.svelte.ts shared rune-based state
  lib/theme.css       design tokens (dark, Supabase-flavoured)
                      — a shell plugin may repaint these; see below
  lib/slots.ts        the frontend slot registry (order-independent)
  lib/plugin-host.ts  runs plugin JS, mounts components into slots
  routes/             one page per panel + a sidebar layout
  routes/plugin-window/  what a plugin-owned window renders
src-tauri/            Rust backend
  src/studio.rs       Studio: owns the cordis Context + WasmHost,
                      persistence, the auto-reload watcher, and the
                      startup-window decision
  src/commands.rs     the #[tauri::command] surface
  src/lib.rs          boot + wire the command handler
  tests/studio.rs     headless integration tests (no window)
```

### Slots and the shell

The app opens five built-in slots (`sidebar.items`, `settings.tabs`,
`dashboard.cards`, `agent.actions`, `plugin.detail`) and a plugin may open its
own. The built-in UI is a fallback: when a shell plugin owns the launch view,
the user sees the shell.

The shell plugin's own slot surface is 17 regions of HanaAgent's chrome
(titlebar, sidebar, conversation, preview, rail). Its documentation, layout and
resize behaviour live with the plugin — see
[`plugins/hana-shell/README.md`](../wasm-plugin-host/plugins/hana-shell/README.md)
and [`docs/仿照-openhanako-外壳.md`](../wasm-plugin-host/docs/仿照-openhanako-外壳.md).

Two tests here guard the shell specifically, because a failure in either is
**invisible at runtime**: a contribution to a declared-but-unmounted slot goes
nowhere, and a module the entry requires but the manifest forgets ships as an
empty space.


## Persistence

Desired state lives in `<app-data>/studio.json` — in the **host's own**
`wasm_plugin_host::Config` format, so it is interoperable with the host CLI:

```json
{
  "cache": { "dir": "cwasm-cache", "enabled": true },
  "plugins": {
    "greet": { "path": "/abs/hello_rust.wasm", "enabled": true,
               "config": { "greeting": "Hi" } }
  }
}
```

Every load / unload / enable / config change writes the file atomically.
Enabled plugins are **loaded on boot**, so the app comes back the way you left
it. A broken or missing persisted plugin is skipped, never fatal to boot.

## Chat (agent loop)

The **Chat** page drives dsh's agent loop directly: create an agent, send a
message, and watch the turn run. Tool calls the model requests are dispatched
into the mounted WASM plugins, and their results appear inline in the
transcript alongside reasoning blocks.

The `mock` provider route always works — it echoes input, and (as the backend
tests show) is enough to exercise a full tool call into a `.wasm` guest.

### Sessions survive a restart

Conversations are written to `<app-data>/sessions/<id>.jsonl` by **dsh's own**
JSONL persistence (`BaseConfig::store_dir`), not by hand-rolled code — so the
format is dsh's, and the CLI's `transcript` command reads the same files.

On boot the session list shows both the agents running now **and** the sessions
left on disk by previous runs. They are marked as *not live* (a hollow dot),
because a stored session has no agent behind it yet: opening one calls
`resume_session`, which seeds a live agent from the stored event log, and from
then on it behaves like any other session.

Two details worth knowing:

* **Seeding replays the log; it does not re-run tools.** The event history is
  restored, so the derived messages come back without any tool call re-executing.
  Seeded events are not re-broadcast to the persistence backend, so resuming
  does not duplicate the transcript on disk — there is a test asserting exactly
  that, across a restart, because the bug is invisible within one process.
* **The working directory is not restored.** dsh stores `cwd` in the session
  *header*, and the JSONL backend writes only events. A resumed session's tools
  therefore run without a cwd; guessing one would look right and run tools in
  the wrong place. The model configuration *is* recovered, from the
  `RequestHeader` each turn appends.

### Wiring a real model

Put an OpenAI-compatible endpoint under `extra.llm` in `studio.json`, then use
its key as the agent's provider route:

```json
{
  "extra": {
    "llm": {
      "providers": {
        "deepseek": {
          "base_url": "https://api.deepseek.com/v1",
          "api_key": "sk-…",
          "model": "deepseek-chat"
        }
      }
    }
  }
}
```

Then **New agent → provider `deepseek`, model `deepseek-chat`**. Providers come
from `dsh-rs`'s OpenAI adapter, so any OpenAI-compatible service works
(DeepSeek, OpenRouter, a local vLLM, …).

> The API key sits in `studio.json` as plain text. That is fine for a local
> single-user app; do not sync that file anywhere.

## Auto-reload

Toggle **auto-reload** on the Plugins page. The backend then watches each
enabled plugin's `.wasm` (via `notify`, with an mtime fallback) and
**hot-swaps it in place on rebuild** — atomically: if the new build is broken,
it is rejected and the running plugin keeps serving. Reloads are reported to the
UI over `studio://plugins-changed`, including rejections.

This is the same stage-then-commit guarantee as the host CLI, driven here from
the GUI.

## Develop

```sh
pnpm install
pnpm tauri dev            # app with hot-reload on the frontend
```

Checks:

```sh
pnpm check                          # svelte-check + tsc
pnpm test                           # frontend unit tests (incl. a command-surface guard)
cd src-tauri && cargo test          # backend integration tests
cd src-tauri && cargo clippy --all-targets
```

> Run the Rust tests with **no dev app running**: `tauri dev` compiles with
> `--no-default-features`, and sharing one `target/` between two feature sets
> produces spurious "two different versions of `serde_json`" errors that look
> like a dependency problem and are not.

## Build

```sh
pnpm tauri build          # .app / .dmg in src-tauri/target/release/bundle
```

## Getting a plugin to load

The backend reads `plugins` from the app-data dir (shown on the **Overview**
page). To try one now, load the sample from the sibling repo:

```sh
cd ../wasm-plugin-host
cargo build --release -p hello-rust --target wasm32-wasip1
```

Then in the app: **Plugins → Load plugin**, slot `greet`, path
`…/wasm-plugin-host/target/wasm32-wasip1/release/hello_rust.wasm`. The
**Validate** button checks the file without loading it. Or drop `.wasm` files
into the plugins directory and use **Discover** to list and load them.

## ⚠️ Capability model: trusted plugins, maximum permission

This is a **decision**, not an unfinished item. A plugin gets preopened access to
`/` (read/write anywhere the app can) plus `host.http_fetch` (the host makes the
request on its behalf). The point of the WASM boundary here is **lifecycle and
isolation from crashes** — a plugin can be unloaded for real, and a bad build
cannot take the app down — not sandboxing.

So: load **your own, trusted** `.wasm` only. Not untrusted third-party plugins.

What makes the WASM boundary still worth having, rather than an in-process
dylib: unloading really releases the code and its linear memory, and a broken
rebuild is rejected while the old one keeps serving. Those are the properties
`dlopen` cannot give. If third-party plugins ever become a goal, the host repo's
P0 lists what a real sandbox would need (fuel, memory ceilings, a preopen
whitelist) and keeps its acceptance criteria.

See the host repo's [`docs/已知问题.md` §1.1](../wasm-plugin-host/docs/已知问题.md)
for the reasoning, and note the **Capabilities page** restates this in-app.

## Notes on the Rust side

* `Studio` is Tauri **managed state**; every type it holds is `Send + Sync`
  (asserted in the host crate's tests), so no wrapper gymnastics are needed.
* Guest calls are serialised behind the registry mutex, and the lock is **never
  held across an `.await`**. Installing/unmounting a slot is `async` because it
  drives cordis fibers; tool calls hop to the blocking pool.
* The harness is booted on Tauri's **global** async runtime, because cordis
  drives each fiber with a tokio task that must outlive boot.
* Slots are mounted with `WasmSlotPlugin::keeping_loaded`, so disposing a fiber
  only *deactivates* the slot in the registry — it does not destroy the guest
  instance. That separation is what makes a hot reload possible (dispose the
  fiber → swap the code → remount); the studio unloads the guest explicitly.

## Native Stage A Agent controls (2026-10-09)

Studio now exposes authenticated Tauri commands for per-session thinking, permission, Memory, primary Agent selection and versioned per-Agent configuration. The settings are stored atomically at `<app-data>/agent-controls.json`; pre-existing JSONL transcripts are never rewritten by config edits. The plugin command bridge must require an explicit `ok: true` acknowledgement for every mutation, including matching target ID and revision.

The vendored `dsh-rs` consumes controls at **execution time**: supported OpenAI reasoning models receive `reasoning_effort` (with `extra-high` mapped to `xhigh`); the tool registry checks the session policy before every tool hook or dispatch; enabled, explicitly configured `memoryNotes` are appended only to that Agent's request system context. `read_only` rejects side-effect/unknown tools, `ask` returns `APPROVAL` for them, and `operate`/`auto` permit normal tool execution. In `ask` mode, side-effectful calls enter a 90-second single-use approval queue. The UI must display exact tool arguments and explicitly confirm/deny. Timeout, cancellation, identity mismatch and policy changes deny the call without executing it. Memory includes durable **per-Agent notes** and an explicit opt-in shared fact bank. Only direct user messages starting `请记住：`, `记住：`, `Remember:` or `remember:` are extracted. Both source and recipient Agents must opt into shared memory before writing or reading it. Shared facts can be reviewed and deleted. The Memory toggle is meaningful when memory notes exist; default notes are empty. Reasoning effort is only available for models known to support it; generic models are locked rather than receiving unsupported API parameters.

New, resumed, and forked Agents receive their own session-scoped control snapshots. `get_agent_config` returns `revision` and configuration; `patch_agent_config` rejects stale revision writes and unsupported settings. Provider/model updates go through Studio's existing live model rebind (never just setting the UI field). `switch_primary_agent` only accepts a live Agent and persists the selected ID; the plugin uses that choice when selecting the current Agent. Existing Studio installations with no control file start from conservative `ask`, empty Memory and `off` Thinking.

Known limitations: the standalone upstream agent identity/Settings UI includes fields not implemented by the Studio Agent runtime; those unrelated PUT fields fail closed. General-purpose semantic extraction, AI-generated automatic memory suggestions, and cross-app knowledge indexing remain separate future enhancements; all current shared-memory capture is explicit user-directed. The approval grants one exact call, never a permanent tool permission. This is not a security boundary for *direct, user-initiated* Tauri calls outside an Agent's tool execution context.

## Phase B — scheduler, project catalog, managed attachments and Shell confinement

Studio persists the Phase B state at `<app-data>/stage-b.json` and exposes authenticated native Tauri commands to list/write the project folder catalog and session assignments using a revision-checked compare-and-swap; list/mutate/run automations; and list/read/delete only files managed under `<app-data>/session-files/<session-id>/`. Project updates cannot acknowledge a stale revision, assignments referencing deleted projects are rejected, and the managed attachment API rejects forged IDs, symlinks and session mismatches. Browser `localStorage` is not the source of truth in the upgraded native project route; **previous browser-only project data is not automatically migrated**.

The automation scheduler is started by the actual Studio application boot, ticks every 20 seconds, and persists due-task advancement before submitting the prompt as a native `send_message` to a live Agent. It supports one-shot RFC3339/epoch timestamps, intervals of one minute through one year, and five-field cron schedules evaluated at a saved fixed local UTC offset; it does not depend on an open OpenHanako tab. It **does** depend on the Studio process staying running and is not a separate always-on OS daemon. Missed enabled `at`/`every` jobs can run on a subsequent Studio boot if their due timestamp was persisted; a recurring task reschedules forward rather than flooding catch-up runs. No claiming success if the actual Agent message fails; the last error is persisted. Automation execution uses the target Agent's configured model; distinct per-job model override/plugin action execution is not implemented by this native scheduler and should not be advertised as such. Time-zone offsets are fixed at creation/update time (not DST-aware IANA time zones).

The vendored `dsh-rs` `bash` tool now requires an existing, authorized working directory and cannot run if no roots are assigned. On macOS it invokes `/usr/bin/sandbox-exec` with a default-deny OS sandbox, permitting file reads only from runtime system directories and granted workspace roots, writes only inside granted workspace roots, and no network access. The sandbox policy is inherited by subprocesses; redirection, symlinks or `cd` alone cannot widen the OS policy. On non-macOS hosts (or macOS without `sandbox-exec`) the tool fails closed instead of falling back to unrestricted `sh -c`. The local TUNL runner's nested `sandbox-exec` aborted before executing a test command; fail-closed unit tests pass, but **successful confined command execution in a normal installed macOS Studio still requires on-device acceptance testing**. This is a boundary around *Agent tool execution*, not direct manually initiated Tauri host shell calls.

Native Rust tests check project revision conflicts, persisted schedules, cron/interval validation, attachment identity/read/delete, and shell denial without authorized roots. Run `cargo test --manifest-path src-tauri/Cargo.toml --lib` plus the vendored `dsh-rs` library tests; the OpenHanako plugin has separate bridge and DOM tests.

## Phase C reliability improvements (2026-10-09)

Native automations now use per-job execution permits to prevent overlapping manual/scheduled runs. Each attempt is durably written **before** sending a prompt, with an explicit interrupted status until the native Agent call completes. Success timestamps are only written after successful execution; failures are retained as `lastError` rather than pretending to be completed. Scheduled one-shot jobs are disabled after their due occurrence (their next time is legitimately `null`). Other schedules continue normally if an individual job is malformed; an invalid due job is disabled with its own error. Restarting Studio can resume a cold explicitly assigned Agent or a persisted primary Agent when dispatching a scheduled prompt. The scheduler exits when Studio's shutdown watcher stops. Job identifiers avoid collisions even when jobs are removed and re-created rapidly; persisted stage-b writes flush the file before atomic replacement, preserving a consistent previous state if writing is interrupted.

**Delivery semantics:** the scheduler persists due-date advancement before dispatch to avoid duplicate execution on restart, so an abrupt process crash between advancement and Agent submission can cause one missed occurrence rather than a duplicate. There is no distributed exactly-once guarantee. The saved `lastAttemptAt`, `lastRunAt`, and `lastError` expose attempted/completed/failed work. The new native Rust tests cover one-shot due handling, an independent malformed schedule, concurrent execution locks, status persistence and a partially written temporary file.

GitHub-hosted CI is **not** used for this development workflow. Tests are run only with local Cargo/Node/TypeScript commands in the user's own machine. The `wasm-plugin-host` workflow is configured separately to avoid automatic push and pull-request test triggers.

### Stage C dispatch reservation and attachment hardening (2026-10-09)

Due scheduler jobs now reserve a one-job execution permit **inside the same critical section** that commits the next due timestamp, before any async task is spawned. This prevents a simultaneous manual `run-now` from stealing the lock and silently losing the already-claimed scheduled occurrence. At-most-once dispatch still allows a missed occurrence after sudden process termination; it does not promise exactly-once LLM completion. The native result status is now `dispatched` (accepted by Agent), not `success` (completed); `lastRunAt` is the last successful dispatch timestamp.

The managed upload/read/delete namespace now verifies that neither the shared `session-files` directory nor the active session's directory is a symlink outside app-data. Managed uploads create a fresh file instead of overwriting an existing path and synchronize the bytes. These checks address ordinary symlink escape attempts; a hostile OS-level race between validation and file opening remains out of scope for these path-based interfaces and would require directory-descriptor-relative APIs for a stronger sandbox boundary. Native tests exercise per-session/root symlink replacement, id validation and execution lease exclusivity.

The OpenHanako frontend repository has removed its GitHub Actions CI workflow entirely. Development verification uses only local `scripts/verify-openhanako-local.sh` through the user's Mac Runner.

### Stage C cross-window locks and managed attachment read safeguards (2026-10-09)

`get_automation_jobs` exposes the native transient `running` flag to all connected windows. `mutate_automation_job` now uses the same `running -> stage_b` lock order as the scheduler's due reservations; editing, toggling, or removing a running job fails rather than changing a task while its previous prompt is being dispatched. The frontend polls this status only while its Automation panel is visible and disables conflicting controls. The flag represents native **dispatch in progress**, not LLM reasoning or tool execution completion.

For managed session attachment reads, macOS and Linux perform a no-follow file open and verify that the opened file still matches the inspected device/inode, before reading at most 20 MiB + 1 byte and rejecting oversized content. This defends against many final-path symlink swaps and file growth beyond the trusted metadata size. The previously documented directory path race still requires openat-style directory-descriptor-relative operations if untrusted local processes can concurrently replace parent path components. Tests cover over-limit sparse attachments and symlinks without reading the external target. No GitHub-hosted CI is involved in this batch.

### Stage C native scheduler draft interop (2026-10-09)

The Studio-native automation store now treats an empty prompt as a valid **disabled draft** (for the OpenHanako Create Automation editor), but refuses to enable or run one until its prompt has been filled. No empty task can be scheduled or executed. The scheduler accepts numeric milliseconds for `every`; the frontend native bridge converts legacy Hana minute strings into that unit and bounds them before dispatch. Unsupported per-job model and executor overrides fail explicitly rather than returning false-positive persistence success; prompt execution uses the Agent's configured model. Native and renderer regressions cover these cases, all run on the local Mac without GitHub CI.

### Stage C: descriptor-relative managed attachments and crash-state reconciliation (2026-10-09)

On macOS/Unix, the native managed-attachment code now opens the trusted app-data base and walks `session-files/<Agent>` using directory-descriptor-relative `openat` with `O_DIRECTORY|O_NOFOLLOW`, never trusting those mutable path components again for file operations. Listing uses an independent `fdopendir` stream, reading opens managed files with `O_NOFOLLOW` and bounded reads, deletion uses `unlinkat` (which does not follow a swapped symbolic link), and upload uses `openat` with `O_EXCL|O_CREAT|O_NOFOLLOW` plus fsync. This closes the parent-directory rename/symlink redirection window **inside these managed attachment operations**. Non-Unix platforms retain the earlier path-validated implementation. The returned filesystem upload path might also be consumed by another component: the new boundary applies to these dedicated native attachment commands and does not magically sandbox arbitrary external consumers of that path. Tests deterministically rename the managed parent and replace it with a symlink after obtaining the descriptor, verifying reads, enumerations, creations and deletions remain confined to the original directory. Final files remain capped at 20 MiB.

Native scheduled Agent submissions now persist a `lastDispatchState` (`dispatching`, `dispatched`, `failed`, `interrupted`) in addition to the attempt time, acknowledged send time and error text. On Studio restart, any task still persisted as `dispatching` is atomically marked `interrupted` with **unknown delivery outcome**, not silently treated as a successful run or automatically replayed. This reconciliation persists before scheduling resumes, and retains the last known successful dispatch timestamp. An Agent's model/tool run finishing is still distinct from message dispatch, and cannot yet be claimed by this acknowledgement. The OpenHanako automation card surfaces the restart warning, with React and native persistence tests.

Local verification remains through `scripts/verify-openhanako-local.sh`; GitHub Actions workflows are absent and must not be recreated.

### Native session trajectory (2026-10-09)

Studio now exports `session_trajectory(agentId,before?,limit?)`, a bounded **read-only native Session Event ledger** for the OpenHanako conversation `Trajectory` tab. It projects real event `seq` and millisecond timestamps, turn/step boundaries, USER/ASSISTANT/SYSTEM/TOOL/CONTEXT/MODEL records, paired tool/turn/step durations, and recorded Token usage. If persisted token-delta events exist it calculates actual TTFT and decode intervals; otherwise those fields are absent. It does not fabricate throughput or start/end times. Returned text is capped and embedded image data URLs are omitted. Requests are restricted to authorized agent/session IDs, default to the last 200 records, and support `before` cursor pagination up to 500 records/page. The feature requires a rebuilt/restarted Studio native host; physical macOS GUI acceptance and the advanced DeepSeek zoom/subtool functionality remain future work. Local Rust unit + actual WASM tool integration tests cover its contract; **no GitHub CI is configured or used**.
