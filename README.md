# dsh-wasm-studio

A **Tauri desktop app** that composes a [`dsh-rs`](https://crates.io/crates/dsh-rs)
agent harness with the [`wasm-plugin-host`](../wasm-plugin-host) WASM plugin
runtime, and puts a Supabase-style admin panel in front of it.

```
┌──────────────────────────────────────────────────────────────┐
│  Svelte 5 (SvelteKit, adapter-static) — admin panel          │
│  Overview · Plugins · Tools · Services · Logs · Capabilities │
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

## Layout

```
src/                  SvelteKit frontend
  lib/api.ts          typed wrappers over every Tauri command
  lib/state.svelte.ts shared rune-based state
  lib/theme.css       design tokens (dark, Supabase-flavoured)
  routes/             one page per panel + a sidebar layout
src-tauri/            Rust backend
  src/studio.rs       Studio: owns the cordis Context + WasmHost,
                      persistence, and the auto-reload watcher
  src/commands.rs     the #[tauri::command] surface
  src/lib.rs          boot + wire the command handler
  tests/studio.rs     headless integration tests (no window)
```

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
cd src-tauri && cargo test          # backend integration tests
cd src-tauri && cargo clippy --all-targets
```

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

## ⚠️ No capability/permission model yet

Plugins currently receive **full WASI** — arbitrary file read/write and network
access. This app is for **your own, trusted** `.wasm` only. Do not load
untrusted plugins. (The host repo's roadmap item P0 tracks this; the
Capabilities page restates the warning in-app.)

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
