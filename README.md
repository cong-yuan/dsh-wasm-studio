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
