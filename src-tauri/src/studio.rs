//! The Studio: the long-lived state every Tauri command operates on.
//!
//! It owns the two halves of the system and keeps them in step:
//!
//! * a **dsh-rs harness** (`cordis::Context` with the base bundle installed) —
//!   the agent loop, sessions, tool registry, LLM seam;
//! * a **WASM plugin host** ([`WasmHost`]) mounted into that harness, so each
//!   WASM slot is its own cordis plugin (own fiber, `inject`, `provide`).
//!
//! ## Why the studio drives the primitives directly
//!
//! `dsh_wasm_host::install` loads every guest *and* mounts every fiber in one
//! shot. The studio needs finer control than that: on a hot reload it must
//! **remount a slot's cordis fiber without re-loading the guest** (the guest is
//! already loaded, and swapping its code is `WasmHost::reload`'s job). So the
//! studio composes the same exported building blocks `install` uses —
//! [`FlowBridgePlugin`] once, then a [`WasmSlotPlugin`] per slot — and owns the
//! fiber handles itself.
//!
//! ## Locking discipline
//!
//! `WasmHost` is `Arc<Mutex<Registry>>` internally and `Registry` is `Send` but
//! not `Sync`, so guest calls serialise behind that mutex. The rule here is:
//! **never hold a lock across an `.await`**. `mounted` and `config` guards are
//! always dropped before awaiting a fiber.
//!
//! ## Runtime context (important)
//!
//! dsh and cordis both use a **bare `tokio::spawn`** on a few paths — the agent
//! driver (`loop_driver::spawn_driver`), fire-and-forget event dispatch
//! (`cordis::events::emit`), and fiber convergence
//! (`cordis::fiber::request_epoch`). A bare spawn **panics with "there is no
//! reactor running"** outside a tokio runtime context.
//!
//! A *synchronous* Tauri command runs with no such context. So:
//!
//! * commands that can reach a spawn are `async fn` (Tauri then drives them on
//!   its runtime), and
//! * the studio methods themselves also guard with [`in_runtime`], so the
//!   library is correct even when called off-runtime.
//!
//! This mattered in practice: creating an agent from the GUI crashed the app
//! until both halves were fixed.
//!
//! ## Persistence
//!
//! Desired state lives in `<app-data>/studio.json`, in the *host's own*
//! [`wasm_plugin_host::Config`] format — so the file is interoperable with the
//! host CLI's supervisor, not a bespoke schema. Enabled plugins are loaded on
//! boot; every mutation writes the file back.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context as _, Result};
use dsh_wasm_host::{FlowBridgePlugin, WasmHost, WasmSlotPlugin};
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter};
use wasm_plugin_host::{Config, PluginEntry};

/// A callback the studio fires when something changes, so the UI can refresh
/// and the watcher can report what it did. Boxed so the Tauri layer can forward
/// to the frontend while tests can just record.
pub type ChangeHook = Arc<dyn Fn(StudioEvent) + Send + Sync>;

/// Events the studio emits to its [`ChangeHook`].
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StudioEvent {
    /// A plugin was hot-reloaded, with its new tool surface.
    Reloaded { slot: String, tools: Vec<String> },
    /// A watched rebuild was rejected (broken build kept the old plugin alive).
    ReloadFailed { slot: String, error: String },
    /// Plugin set / state changed (load, unload, enable, config).
    Changed,
}

struct Shared {
    ctx: cordis::Context,
    host: WasmHost,
    /// The Tauri app handle, kept so the studio can open/close plugin windows
    /// (and clean them up when a plugin stops). `None` in headless tests.
    app: Mutex<Option<tauri::AppHandle>>,
    /// Windows currently open on behalf of plugins: label -> owning slot.
    open_windows: Mutex<std::collections::HashMap<String, String>>,
    /// Params to hand a window when it opens or is next focused: label -> JSON.
    /// Cleared once delivered.
    pending_params: Mutex<std::collections::HashMap<String, serde_json::Value>>,
    /// Windows a plugin asked to open, that could not be opened **yet** because
    /// no app handle existed. Boot autoloads plugins before the Tauri handle is
    /// installed, so `open: "auto"` windows are requested while there is still
    /// nothing to open them with. Keeping the *intent* — not just the effect —
    /// lets the startup path replay it once the handle exists.
    wanted_windows: Mutex<Vec<String>>,
    /// slot -> its cordis fiber (the slot's own plugin instance).
    mounted: Mutex<Vec<(String, FiberHandle)>>,
    /// Agents **resumed from a stored session**, keyed by id.
    ///
    /// dsh's `AgentRegistry` only has `create`, and `create` always mints an
    /// *empty* session (`CreateSessionOptions { ..Default::default() }`); there
    /// is no `insert`. So an agent built on a session that was *seeded* from
    /// disk (resume) cannot live in the registry, and is held here instead.
    ///
    /// Everything that reads an agent by id (`agent`, `list_agents`,
    /// `dispose_agent`) consults this map, so a resumed agent is
    /// indistinguishable from a created one to the rest of the studio.
    resumed: Mutex<std::collections::HashMap<String, Arc<dsh_rs::core::Agent>>>,
    /// Cached rows for the sessions on disk; `None` means "not scanned yet".
    ///
    /// The scan reads whole `.jsonl` files, and dsh logs **every stream chunk**
    /// as its own line, so a session is not small. The sidebar polls the session
    /// list every few seconds, so without this each poll would re-read every
    /// session file. Invalidated whenever this process changes what is on disk.
    stored_cache: Mutex<Option<Vec<AgentRow>>>,
    /// Handed out to auto-created sessions so their ids cannot collide with
    /// sessions already on disk (see [`Studio::new_session_id`]).
    id_seq: std::sync::atomic::AtomicU64,
    /// A registry used **only** to dispose resumed agents.
    ///
    /// Disposal needs to flip an agent's private `disposed` flag, and the only
    /// public way to do that is `AgentRegistry::dispose`, which acts on any
    /// `&Arc<Agent>` — it does not require the agent to be in its map. The
    /// registry the harness provides is behind a service wrapper whose `dispose`
    /// *does* look in the map, so it is a no-op for a resumed agent. Rather than
    /// build a throwaway registry per disposal, this empty one is built once at
    /// boot for that one call.
    agent_owner: dsh_rs::core::AgentRegistry,
    /// The single shared flow-bridge fiber.
    bridge: Mutex<Option<FiberHandle>>,
    plugins_dir: PathBuf,
    config_path: PathBuf,
    /// The desired state, mirrored to `config_path`.
    config: Mutex<Config>,
    booted: bool,
    // --- auto-reload watcher ---
    watch_stop: AtomicBool,
    watch_handle: Mutex<Option<JoinHandle<()>>>,
    hook: Mutex<Option<ChangeHook>>,
}

/// A cheap-clone handle over the shared state. Everything the commands do goes
/// through here, so the watcher thread can hold its own clone.
#[derive(Clone)]
pub struct Studio {
    shared: Arc<Shared>,
}

type FiberHandle = cordis::FiberHandle;

/// The label Tauri gives the app's own window (see `tauri.conf.json`).
///
/// It is created **hidden** so a plugin owning the launch view never lets the
/// default page flash first; [`Studio::apply_startup_windows`] is what puts a
/// window on screen.
const MAIN_LABEL: &str = "main";

impl Studio {
    /// Build a studio with an explicit log hook and app-data directory.
    ///
    /// This is the real constructor; [`Studio::boot`] is the Tauri wrapper that
    /// supplies an event-emitting hook. Keeping Tauri types out of here lets the
    /// whole boot-and-autoload path be exercised headlessly in tests.
    pub async fn with_hook(
        log_hook: Option<wasm_plugin_host::LogHook>,
        change_hook: Option<ChangeHook>,
        app_data_dir: PathBuf,
    ) -> Result<Self> {
        let plugins_dir = app_data_dir.join("plugins");
        let config_path = app_data_dir.join("studio.json");
        let _ = std::fs::create_dir_all(&plugins_dir);

        // Read the persisted config FIRST: it decides the compile-cache
        // directory, so it must be known before the runtime is built.
        let config = if config_path.exists() {
            Config::load(&config_path).context("reading studio.json")?
        } else {
            // First boot: write a default so the user can find and hand-edit it.
            let cfg = Config {
                cache: Some(wasm_plugin_host::CacheConfig {
                    dir: app_data_dir.join("cwasm-cache").display().to_string(),
                    enabled: true,
                }),
                ..Config::default()
            };
            let _ = cfg.save(&config_path);
            cfg
        };

        // Resolve the cache dir relative to the config file, matching how the
        // host's own supervisor treats config-relative paths.
        let cache_dir = config
            .cache
            .as_ref()
            .filter(|c| c.enabled)
            .map(|c| {
                let p = Path::new(&c.dir);
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    app_data_dir.join(p)
                }
            });

        let ctx = cordis::Context::new();
        let host = WasmHost::with_options(dsh_wasm_host::HostOptions {
            log_capacity: Some(4000),
            echo_stderr: false,
            log_hook,
            cache_dir,
        })
        .context("building the WASM host")?;

        // Boot the dsh harness (agent loop, sessions, tools, llm seam).
        //
        // We pass a real `BaseConfig` rather than using
        // `install_base_default`, which is documented as *"with an in-memory
        // store"*. Two things come from the dsh bundle itself instead of us
        // re-implementing them:
        //
        // * `store_dir` attaches dsh's own JSONL session persistence, so a
        //   conversation survives a restart.
        // * `adapters` mounts each configured OpenAI-compatible endpoint as a
        //   provider route — the same thing we used to do by hand, after boot,
        //   through a second path that could drift from dsh's own.
        boot_harness(&ctx, base_config(&config, &app_data_dir)).await?;

        // Cover the gateway's baked-in "minimal coding agent" persona (9router
        // models answer "你是谁" in that voice, sometimes Vietnamese). A Studio
        // section at order 0 becomes the deployment persona on every turn.
        install_studio_persona(&ctx, &config);

        // Tell the WASM registry which dsh services WASM plugins may inject.
        for svc in dsh_services() {
            host.declare_dsh_service(svc);
        }

        // Mount the shared flow bridge once (it is a host-wide concern).
        let bridge_plugin: Arc<dyn cordis::plugin::Plugin> =
            Arc::new(FlowBridgePlugin::new(host.clone()));
        let bridge = ctx.plugin(bridge_plugin, None);
        join_bounded(&bridge, SETTLE_TIMEOUT, "flow bridge").await?;

        // An empty registry, used only as the vehicle for disposing resumed
        // agents (see `Shared::agent_owner`). Built here because the sessions
        // service it needs exists only after the harness boots.
        let sessions = ctx
            .require::<dsh_rs::api::services::SessionService>(dsh_rs::api::SESSIONS_SERVICE)
            .context("sessions service — is the dsh base bundle installed?")?;
        let agent_owner = dsh_rs::core::AgentRegistry::new(ctx.clone(), (*sessions).clone());

        let studio = Studio {
            shared: Arc::new(Shared {
                ctx,
                host,
                app: Mutex::new(None),
                open_windows: Mutex::new(std::collections::HashMap::new()),
                pending_params: Mutex::new(std::collections::HashMap::new()),
                wanted_windows: Mutex::new(Vec::new()),
                mounted: Mutex::new(Vec::new()),
                resumed: Mutex::new(std::collections::HashMap::new()),
                stored_cache: Mutex::new(None),
                id_seq: std::sync::atomic::AtomicU64::new(0),
                agent_owner,
                bridge: Mutex::new(Some(bridge)),
                plugins_dir,
                config_path,
                config: Mutex::new(config),
                booted: true,
                watch_stop: AtomicBool::new(false),
                watch_handle: Mutex::new(None),
                hook: Mutex::new(change_hook),
            }),
        };

        // Load the persisted desired state.
        studio.autoload().await;
        Ok(studio)
    }

    /// Build the studio in a Tauri app: log lines and change events are
    /// forwarded to the frontend as Tauri events.
    pub fn boot(app: &AppHandle) -> Result<Self> {
        let app_for_logs = app.clone();
        let log_hook: wasm_plugin_host::LogHook =
            Arc::new(move |rec: &wasm_plugin_host::LogRecord| {
                let _ = app_for_logs.emit("studio://log", rec);
            });

        let app_for_changes = app.clone();
        let change_hook: ChangeHook = Arc::new(move |ev: StudioEvent| {
            let _ = app_for_changes.emit("studio://plugins-changed", &ev);
            // A reload changes tools/services, so clients should refetch.
            let _ = app_for_changes.emit("studio://changed", ());
        });

        let app_data = app_data_dir(app);
        // Blocks on Tauri's *global* runtime, which stays alive for the whole
        // app — so the fiber tasks booted here keep running.
        let studio = tauri::async_runtime::block_on(Self::with_hook(
            Some(log_hook),
            Some(change_hook),
            app_data,
        ))?;

        // Remember the handle so plugin windows can be created/closed later.
        *studio.shared.app.lock().unwrap() = Some(app.clone());

        // Now that a handle exists, decide what the user actually sees: a
        // plugin's startup window if one claims it, otherwise the app window.
        // This must come after the handle is installed — plugin windows cannot
        // be created without it, and autoload (inside `with_hook`) has already
        // queued the `auto` windows that were requested before that point.
        studio.apply_startup_windows(app);

        // Start the filesystem watcher (best-effort; the app works without it).
        studio.start_watch();
        Ok(studio)
    }

    // -----------------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------------

    pub fn host(&self) -> &WasmHost {
        &self.shared.host
    }

    pub fn ctx(&self) -> &cordis::Context {
        &self.shared.ctx
    }

    pub fn plugins_dir(&self) -> &Path {
        &self.shared.plugins_dir
    }

    pub fn config_path(&self) -> &Path {
        &self.shared.config_path
    }

    /// A copy of the persisted desired state.
    pub fn config(&self) -> Config {
        self.shared.config.lock().unwrap().clone()
    }

    pub fn is_mounted(&self, slot: &str) -> bool {
        self.shared
            .mounted
            .lock()
            .unwrap()
            .iter()
            .any(|(s, _)| s == slot)
    }

    /// Is the auto-reload watcher running?
    pub fn watching(&self) -> bool {
        !self.shared.watch_stop.load(Ordering::SeqCst)
            && self.shared.watch_handle.lock().unwrap().is_some()
    }

    fn fire(&self, ev: StudioEvent) {
        if let Some(hook) = self.shared.hook.lock().unwrap().as_ref() {
            hook(ev);
        }
    }

    // -----------------------------------------------------------------------
    // Mount / unmount / reload
    // -----------------------------------------------------------------------

    /// Load a guest into the registry (no-op if already loaded).
    /// Async: instantiating runs guest code, so it must be off the runtime.
    async fn ensure_loaded(&self, slot: &str, path: &str, config: Value) -> Result<()> {
        if self.shared.host.is_loaded(slot) {
            return Ok(());
        }
        let host = self.shared.host.clone();
        let slot = slot.to_string();
        let path = path.to_string();
        off_runtime(move || {
            host.load(&slot, &path, config)
                .map(|_report| ())
                .with_context(|| format!("loading `{path}` into slot `{slot}`"))
        })
        .await
    }

    /// Mount the cordis fiber for an **already-loaded** slot. Its `inject` list
    /// may leave the fiber PENDING — that is correct, not a failure.
    async fn mount_fiber(&self, slot: &str) -> Result<()> {
        // `keeping_loaded`: disposing this fiber must only deactivate the slot
        // in the registry, never destroy the guest instance — the studio owns
        // the guest lifecycle (reload swaps its code; unmount unloads it).
        let plugin: Arc<dyn cordis::plugin::Plugin> = Arc::new(WasmSlotPlugin::keeping_loaded(
            slot.to_string(),
            self.shared.host.clone(),
        ));
        let fiber = self.shared.ctx.plugin(plugin, None);
        // Surface only a *failed* startup; PENDING convergence is fine.
        join_bounded(&fiber, SETTLE_TIMEOUT, &format!("slot `{slot}`")).await?;
        self.shared
            .mounted
            .lock()
            .unwrap()
            .push((slot.to_string(), fiber));
        Ok(())
    }

    /// Dispose a slot's cordis fiber without touching the guest registry.
    /// Returns the tools that were on it (for reporting).
    async fn dispose_fiber(&self, slot: &str) -> Vec<String> {
        let taken = {
            let mut guard = self.shared.mounted.lock().unwrap();
            guard
                .iter()
                .position(|(s, _)| s == slot)
                .map(|i| guard.remove(i))
        };
        let tools = self
            .shared
            .host
            .list_tools()
            .into_iter()
            .filter(|t| t.slot == slot)
            .map(|t| t.name)
            .collect();
        if let Some((_, fiber)) = taken {
            fiber.dispose().await;
        }
        tools
    }

    /// Load and mount a plugin, persisting it to `studio.json`.
    pub async fn mount_slot(&self, slot: &str, path: &str, config: Value) -> Result<()> {
        self.mount_inner(slot, path, config.clone()).await?;
        // Persist desired state.
        let mut cfg = self.shared.config.lock().unwrap();
        cfg.plugins.insert(
            slot.to_string(),
            PluginEntry {
                path: path.to_string(),
                enabled: true,
                watch: None,
                config: if config.is_null() { None } else { Some(config) },
                restart_on_config: false,
            },
        );
        drop(cfg);
        self.save()?;
        self.fire(StudioEvent::Changed);
        Ok(())
    }

    /// The mount path used by autoload/watcher: no persistence side effects.
    pub async fn mount_inner(&self, slot: &str, path: &str, config: Value) -> Result<()> {
        // Replace if present, so this doubles as "reload from path".
        if self.is_mounted(slot) {
            self.dispose_fiber(slot).await;
        }
        self.ensure_loaded(slot, path, config).await?;
        self.mount_fiber(slot).await?;
        // A plugin may ask for a window to appear with it (`open: "auto"`).
        self.open_auto_windows(slot);
        Ok(())
    }

    /// Unload a slot and drop its fiber, removing it from `studio.json`.
    /// **Stop** a slot: unload the guest and dismount its fiber, but **keep it
    /// in the config** so it stays listed and can be started again.
    ///
    /// This used to delete the config entry, which made "stop" indistinguishable
    /// from "forget": the plugin vanished from the UI and discovery would not
    /// offer it back (its path was still configured). Stopping is not removing.
    pub async fn unmount_slot(&self, slot: &str) -> Result<()> {
        self.unmount_inner(slot).await?;
        // Mark disabled rather than deleting, if we know about it.
        {
            let mut cfg = self.shared.config.lock().unwrap();
            if let Some(entry) = cfg.plugins.get_mut(slot) {
                entry.enabled = false;
            }
        }
        self.save()?;
        self.fire(StudioEvent::Changed);
        Ok(())
    }

    /// **Remove** a slot entirely: stop it and delete it from the config.
    /// Use this when you want the plugin forgotten, not merely stopped.
    pub async fn remove_slot(&self, slot: &str) -> Result<()> {
        self.unmount_inner(slot).await?;
        {
            let mut cfg = self.shared.config.lock().unwrap();
            cfg.plugins.remove(slot);
        }
        self.save()?;
        self.fire(StudioEvent::Changed);
        Ok(())
    }

    /// Unmount without persisting (used by the watcher / reload path).
    pub async fn unmount_inner(&self, slot: &str) -> Result<()> {
        // Close windows first: a window showing a dead plugin's component would
        // render nothing.
        self.close_plugin_windows(slot);
        self.dispose_fiber(slot).await;
        if self.shared.host.is_loaded(slot) {
            let host = self.shared.host.clone();
            let slot = slot.to_string();
            off_runtime(move || host.unload(&slot)).await?;
        }
        Ok(())
    }

    /// Enable/disable a slot's persisted state, loading or unloading to match.
    pub async fn set_enabled(&self, slot: &str, enabled: bool) -> Result<()> {
        let path = {
            let mut cfg = self.shared.config.lock().unwrap();
            let entry = cfg
                .plugins
                .get_mut(slot)
                .ok_or_else(|| anyhow::anyhow!("slot `{slot}` is not in the config"))?;
            entry.enabled = enabled;
            entry.path.clone()
        };
        if enabled {
            let config = self
                .config()
                .plugins
                .get(slot)
                .and_then(|e| e.config.clone())
                .unwrap_or(Value::Null);
            self.mount_inner(slot, &path, config).await?;
        } else {
            self.unmount_inner(slot).await?;
        }
        self.save()?;
        self.fire(StudioEvent::Changed);
        Ok(())
    }

    /// Hot-reload a slot's code (`stage-then-commit`), then remount its fiber so
    /// the cordis side sees the new tool/service surface.
    ///
    /// If the new build is rejected, the running plugin is left intact and the
    /// error is returned — the studio never tears down a working plugin for a
    /// broken build.
    pub async fn reload_slot(&self, slot: &str) -> Result<Vec<String>> {
        let path = self
            .shared
            .host
            .registry()
            .lock()
            .map(|r| r.slot_path(slot))
            .map_err(|e| anyhow::anyhow!("registry mutex poisoned: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("slot `{slot}` is not loaded"))?;
        let path = path.display().to_string();

        // 1. Take the cordis fiber down (removes its tools/services from dsh).
        self.dispose_fiber(slot).await;

        // 2. Atomic code swap. On failure the OLD code stays loaded.
        let host = self.shared.host.clone();
        let slot_owned = slot.to_string();
        let result = off_runtime(move || host.reload(&slot_owned, &path, None)).await;

        // 3. Remount either way, so the registry and the harness agree.
        self.mount_fiber(slot).await?;

        match result {
            Ok(_) => {
                let tools = self
                    .shared
                    .host
                    .list_tools()
                    .into_iter()
                    .filter(|t| t.slot == slot)
                    .map(|t| t.name)
                    .collect::<Vec<_>>();
                self.fire(StudioEvent::Reloaded {
                    slot: slot.to_string(),
                    tools: tools.clone(),
                });
                Ok(tools)
            }
            Err(e) => {
                self.fire(StudioEvent::ReloadFailed {
                    slot: slot.to_string(),
                    error: e.to_string(),
                });
                Err(e)
            }
        }
    }

    /// Push a new config value to a running slot and persist it.
    /// Async: `apply_config` may invoke the guest's `plugin_on_config` hook.
    pub async fn set_slot_config(&self, slot: &str, config: Value) -> Result<bool> {
        let host = self.shared.host.clone();
        let slot_owned = slot.to_string();
        let cfg_in = config.clone();
        let consumed = off_runtime(move || host.apply_config(&slot_owned, cfg_in)).await?;
        {
            let mut cfg = self.shared.config.lock().unwrap();
            if let Some(entry) = cfg.plugins.get_mut(slot) {
                entry.config = if config.is_null() { None } else { Some(config) };
            }
        }
        self.save()?;
        self.fire(StudioEvent::Changed);
        Ok(consumed)
    }

    // -----------------------------------------------------------------------
    // Persistence
    // -----------------------------------------------------------------------



    /// Re-register OpenAI-compatible adapters from `extra.llm.providers`.
    ///
    /// Boot only mounts whatever was in studio.json at startup; the settings UI
    /// writes providers later. Without this, `create_agent("9router", …)` fails
    /// with "no adapter registered" until a full restart.
    pub fn sync_llm_adapters(&self) -> Result<Vec<String>> {
        let llm = self
            .shared
            .ctx
            .require::<dsh_rs::api::services::LlmService>(dsh_rs::api::LLM_SERVICE)
            .map_err(|e| anyhow::anyhow!("llm service unavailable: {e}"))?;

        let cfg = self.shared.config.lock().unwrap().clone();
        let providers = cfg
            .extra
            .as_ref()
            .and_then(|e| e.get("llm"))
            .and_then(|llm| llm.get("providers"))
            .and_then(|p| p.as_object())
            .cloned()
            .unwrap_or_default();

        let current = cfg
            .extra
            .as_ref()
            .and_then(|e| e.get("llm"))
            .and_then(|llm| llm.get("current"))
            .cloned()
            .unwrap_or(serde_json::json!({}));

        let mut live = Vec::new();
        for (name, entry) in &providers {
            if name == "mock" {
                continue;
            }
            let Some(obj) = entry.as_object() else { continue };
            let base_url = obj
                .get("base_url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .trim_end_matches('/');
            if base_url.is_empty() {
                continue;
            }
            let api_key = obj.get("api_key").and_then(|v| v.as_str()).unwrap_or("");
            // Prefer explicit model, then current selection for this provider, then first model_lists entry.
            let mut model = obj
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if model.is_empty() {
                if current.get("provider").and_then(|v| v.as_str()) == Some(name.as_str()) {
                    model = current
                        .get("model")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                }
            }
            if model.is_empty() {
                if let Some(list) = cfg
                    .extra
                    .as_ref()
                    .and_then(|e| e.get("llm"))
                    .and_then(|llm| llm.get("model_lists"))
                    .and_then(|m| m.get(name))
                    .and_then(|v| v.as_array())
                {
                    for item in list {
                        if let Some(s) = item.as_str() {
                            model = s.to_string();
                            break;
                        }
                        if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                            model = id.to_string();
                            break;
                        }
                    }
                }
            }

            let adapter_cfg = serde_json::json!({
                "base_url": base_url,
                "api_key": api_key,
                "model": model,
            });

            // Replace any prior registration for this route.
            llm.unregister_adapter(&[name.as_str()]);
            let adapter = std::sync::Arc::new(
                dsh_rs::llm::adapters::openai::OpenAiAdapter::new(adapter_cfg)
                    .map_err(|e| anyhow::anyhow!("adapter {name}: {e}"))?,
            );
            llm.register_adapter(
                &[name.as_str()],
                adapter as std::sync::Arc<dyn dsh_rs::api::services::LlmAdapterApi>,
            )
            .map_err(|e| anyhow::anyhow!("register {name}: {e}"))?;
            live.push(name.clone());
        }
        Ok(live)
    }

    /// Fetch `/models` from an OpenAI-compatible provider (by id or raw endpoint).
    pub async fn fetch_llm_models(
        &self,
        provider: Option<String>,
        base_url: Option<String>,
        api_key: Option<String>,
    ) -> Result<serde_json::Value> {
        let (url, key, pname) = {
            let cfg = self.shared.config.lock().unwrap();
            let llm = cfg.extra.as_ref().and_then(|e| e.get("llm"));
            let providers = llm.and_then(|l| l.get("providers")).and_then(|p| p.as_object());
            if let Some(name) = provider.as_ref() {
                let entry = providers.and_then(|p| p.get(name)).and_then(|v| v.as_object());
                let bu = base_url
                    .clone()
                    .or_else(|| {
                        entry
                            .and_then(|e| e.get("base_url"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_default();
                let key = api_key.clone().or_else(|| {
                    entry
                        .and_then(|e| e.get("api_key"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                });
                (bu, key.unwrap_or_default(), name.clone())
            } else {
                (
                    base_url.unwrap_or_default(),
                    api_key.unwrap_or_default(),
                    String::new(),
                )
            }
        };

        let base = url.trim().trim_end_matches('/').to_string();
        if base.is_empty() {
            anyhow::bail!("missing base_url");
        }
        let endpoint = format!("{base}/models");
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()?;
        let mut req = client.get(&endpoint);
        if !key.is_empty() {
            req = req.bearer_auth(&key);
        }
        let res = req.send().await?;
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("GET {endpoint} -> {status}: {}", body.chars().take(300).collect::<String>());
        }
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::json!({}));
        let raw_list = parsed
            .get("data")
            .and_then(|v| v.as_array())
            .cloned()
            .or_else(|| parsed.as_array().cloned())
            .unwrap_or_default();

        let mut models = Vec::new();
        for item in raw_list {
            let id = item
                .get("id")
                .or_else(|| item.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if id.is_empty() {
                continue;
            }
            let mut row = serde_json::json!({ "id": id, "name": item.get("name").and_then(|v| v.as_str()).unwrap_or(&id) });
            // Pass through useful metadata when the upstream provides it.
            for key in [
                "context",
                "context_window",
                "contextWindow",
                "max_output",
                "maxOutput",
                "max_tokens",
                "maxTokens",
                "owned_by",
                "type",
                "reasoning",
                "vision",
                "image",
                "video",
                "audio",
            ] {
                if let Some(v) = item.get(key) {
                    row[key] = v.clone();
                }
            }
            // Normalize a few aliases into the shape openhanako's DiscoveredModel expects.
            if let Some(ctx) = item.get("context_window").or_else(|| item.get("contextWindow")) {
                row["context"] = ctx.clone();
                row["contextWindow"] = ctx.clone();
            }
            if item.get("vision").and_then(|v| v.as_bool()) == Some(true)
                || item.get("image").and_then(|v| v.as_bool()) == Some(true)
            {
                row["image"] = serde_json::json!(true);
                row["vision"] = serde_json::json!(true);
            }
            // Some local routers (e.g. 9router) nest flags under `capabilities`.
            if let Some(caps) = item.get("capabilities").and_then(|v| v.as_object()) {
                if caps.get("vision").and_then(|v| v.as_bool()) == Some(true) {
                    row["image"] = serde_json::json!(true);
                    row["vision"] = serde_json::json!(true);
                }
                if caps.get("reasoning").and_then(|v| v.as_bool()) == Some(true) {
                    row["reasoning"] = serde_json::json!(true);
                }
                if caps.get("audioInput").and_then(|v| v.as_bool()) == Some(true)
                    || caps.get("audio").and_then(|v| v.as_bool()) == Some(true)
                {
                    row["audio"] = serde_json::json!(true);
                }
                if caps.get("videoInput").and_then(|v| v.as_bool()) == Some(true) {
                    row["video"] = serde_json::json!(true);
                }
                row["capabilities"] = item.get("capabilities").cloned().unwrap();
            }
            if let Some(owned) = item.get("owned_by") {
                row["owned_by"] = owned.clone();
            }
            models.push(row);
        }

        // Cache discovered list under extra.llm.discovered.<provider>
        if !pname.is_empty() {
            let mut cfg = self.shared.config.lock().unwrap();
            let mut extra = cfg.extra.clone().unwrap_or_else(|| serde_json::json!({}));
            if !extra.is_object() {
                extra = serde_json::json!({});
            }
            let extra_obj = extra.as_object_mut().unwrap();
            let mut llm = extra_obj
                .get("llm")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            if !llm.is_object() {
                llm = serde_json::json!({});
            }
            let llm_obj = llm.as_object_mut().unwrap();
            let mut discovered = llm_obj
                .get("discovered")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            discovered.insert(pname.clone(), serde_json::Value::Array(models.clone()));
            llm_obj.insert("discovered".into(), serde_json::Value::Object(discovered));
            extra_obj.insert("llm".into(), serde_json::Value::Object(llm_obj.clone()));
            cfg.extra = Some(extra);
            drop(cfg);
            let _ = self.save();
        }

        Ok(serde_json::json!({ "models": models, "provider": pname }))
    }

    /// Read the persisted LLM section (`extra.llm`) plus live registered routes.
    ///
    /// `providers` here is the **config** map (base_url / api_key / model). The
    /// live registry may still only know a subset until the app restarts after a
    /// write — see [`Studio::set_llm_config`].
    pub fn llm_config(&self) -> serde_json::Value {
        let cfg = self.shared.config.lock().unwrap();
        let llm = cfg
            .extra
            .as_ref()
            .and_then(|e| e.get("llm"))
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let mut providers = llm
            .get("providers")
            .and_then(|p| p.as_object())
            .cloned()
            .unwrap_or_default();
        // Always surface `mock` so the picker is never empty on a fresh install.
        providers
            .entry("mock".to_string())
            .or_insert_with(|| serde_json::json!({ "model": "mock-1" }));
        let model_lists = llm
            .get("model_lists")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let discovered = llm
            .get("discovered")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let current = llm.get("current").cloned().unwrap_or_else(|| {
            serde_json::json!({ "provider": "mock", "model": "mock-1" })
        });
        let default_provider = llm
            .get("default")
            .and_then(|d| d.as_str())
            .unwrap_or("mock")
            .to_string();
        drop(cfg);
        let registered = self.providers();
        serde_json::json!({
            "providers": providers,
            "model_lists": model_lists,
            "discovered": discovered,
            "current": current,
            "default": default_provider,
            "registered": registered,
        })
    }

    /// Merge a patch into `extra.llm` and persist `studio.json`.
    ///
    /// Patch shape (all fields optional):
    /// ```json
    /// {
    ///   "providers": { "deepseek": { "base_url": "...", "api_key": "...", "model": "..." } | null },
    ///   "model_lists": { "deepseek": ["deepseek-chat"] },
    ///   "current": { "provider": "deepseek", "model": "deepseek-chat" },
    ///   "default": "deepseek"
    /// }
    /// ```
    /// New provider routes only reach the live registry after restart
    /// (`restart_required` is set when a key is not yet registered).
    pub fn set_llm_config(&self, patch: serde_json::Value) -> Result<serde_json::Value> {
        let registered_before = self.providers();
        {
            let mut cfg = self.shared.config.lock().unwrap();
            let mut extra = cfg.extra.clone().unwrap_or_else(|| serde_json::json!({}));
            if !extra.is_object() {
                extra = serde_json::json!({});
            }
            let extra_obj = extra.as_object_mut().unwrap();
            let mut llm = extra_obj
                .get("llm")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            if !llm.is_object() {
                llm = serde_json::json!({});
            }
            let llm_obj = llm.as_object_mut().unwrap();

            if let Some(providers_patch) = patch.get("providers").and_then(|p| p.as_object()) {
                let mut providers = llm_obj
                    .get("providers")
                    .and_then(|p| p.as_object())
                    .cloned()
                    .unwrap_or_default();
                let mut model_lists = llm_obj
                    .get("model_lists")
                    .and_then(|p| p.as_object())
                    .cloned()
                    .unwrap_or_default();
                for (name, value) in providers_patch {
                    // `mock` is built into dsh; never persist it as a real adapter.
                    if name == "mock" {
                        providers.remove(name);
                        continue;
                    }
                    if value.is_null() {
                        providers.remove(name);
                        model_lists.remove(name);
                        continue;
                    }
                    let src = value.as_object().cloned().unwrap_or_default();
                    let mut clean = serde_json::Map::new();
                    // Accept both snake_case (studio) and camelCase (openhanako).
                    for (from, to) in [
                        ("base_url", "base_url"),
                        ("baseUrl", "base_url"),
                        ("api_key", "api_key"),
                        ("apiKey", "api_key"),
                        ("model", "model"),
                    ] {
                        if let Some(v) = src.get(from) {
                            if !v.is_null() {
                                clean.insert(to.to_string(), v.clone());
                            }
                        }
                    }
                    // Preserve previously known fields when the patch is partial.
                    let entry = providers.entry(name.clone()).or_insert_with(|| serde_json::json!({}));
                    if let Some(obj) = entry.as_object_mut() {
                        for (k, v) in clean {
                            obj.insert(k, v);
                        }
                    } else {
                        *entry = serde_json::Value::Object(clean);
                    }
                    if let Some(models) = src.get("models") {
                        let cleaned = match models.as_array() {
                            Some(arr) => {
                                let v: Vec<serde_json::Value> = arr
                                    .iter()
                                    .filter(|m| {
                                        let id = m.as_str().map(|s| s.to_string()).or_else(|| {
                                            m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string())
                                        });
                                        // Drop the auto-seeded "model named like provider".
                                        id.as_deref() != Some(name.as_str())
                                    })
                                    .cloned()
                                    .collect();
                                serde_json::Value::Array(v)
                            }
                            None => models.clone(),
                        };
                        model_lists.insert(name.clone(), cleaned);
                    }
                }
                llm_obj.insert("providers".into(), serde_json::Value::Object(providers));
                llm_obj.insert("model_lists".into(), serde_json::Value::Object(model_lists));
            }

            if let Some(lists) = patch.get("model_lists").and_then(|p| p.as_object()) {
                let mut model_lists = llm_obj
                    .get("model_lists")
                    .and_then(|p| p.as_object())
                    .cloned()
                    .unwrap_or_default();
                for (k, v) in lists {
                    model_lists.insert(k.clone(), v.clone());
                }
                llm_obj.insert("model_lists".into(), serde_json::Value::Object(model_lists));
            }

            if let Some(current) = patch.get("current") {
                llm_obj.insert("current".into(), current.clone());
            }
            if let Some(default) = patch.get("default") {
                llm_obj.insert("default".into(), default.clone());
            }

            extra_obj.insert("llm".into(), serde_json::Value::Object(llm_obj.clone()));
            cfg.extra = Some(extra);
        }
        self.save()?;
        // Mount / refresh adapters so new providers are usable without restart.
        let sync_err = self.sync_llm_adapters().err().map(|e| e.to_string());
        let mut out = self.llm_config();
        let registered = self.providers();
        if let Some(obj) = out.as_object_mut() {
            if let Some(err) = sync_err {
                obj.insert("sync_error".into(), serde_json::json!(err));
            }
        }
        let providers = out
            .get("providers")
            .and_then(|p| p.as_object())
            .map(|m| m.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let needs_restart = providers.iter().any(|p| {
            p != "mock" && !registered.iter().any(|r| r == p) && !registered_before.iter().any(|r| r == p)
        }) || providers.iter().any(|p| p != "mock" && !registered.iter().any(|r| r == p));
        if let Some(obj) = out.as_object_mut() {
            obj.insert("restart_required".into(), serde_json::json!(needs_restart));
            obj.insert("registered".into(), serde_json::json!(registered));
        }
        Ok(out)
    }

    fn save(&self) -> Result<()> {
        let cfg = self.shared.config.lock().unwrap().clone();
        // Write atomically so a crash cannot leave a truncated studio.json.
        let tmp = self.shared.config_path.with_extension("json.tmp");
        cfg.save(&tmp)?;
        std::fs::rename(&tmp, &self.shared.config_path)?;
        Ok(())
    }

    /// Load every enabled plugin from the persisted config. Failures are
    /// tolerated (a missing/broken file must not stop the app booting); the
    /// slot is simply skipped and reported through the return value.
    pub async fn autoload(&self) -> Vec<(String, String)> {
        let entries: Vec<(String, PluginEntry)> = self
            .shared
            .config
            .lock()
            .unwrap()
            .plugins
            .iter()
            .filter(|(_, e)| e.enabled)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut failures = Vec::new();
        for (slot, entry) in entries {
            let path = self.resolve(&entry.path);
            let config = entry.config.clone().unwrap_or(Value::Null);
            if let Err(e) = self
                .mount_inner(&slot, &path.display().to_string(), config)
                .await
            {
                failures.push((slot, e.to_string()));
            }
        }
        failures
    }

    /// Resolve a config path against the app-data directory (mirrors how the
    /// host's own supervisor resolves relative paths).
    fn resolve(&self, p: &str) -> PathBuf {
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.shared
                .config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(path)
        }
    }

    /// Scan the plugins directory for `.wasm` files not already configured.
    /// Find candidate `.wasm` plugins in every place we look.
    ///
    /// Discovery used to scan a single (usually empty) app-data directory, which
    /// meant "Discover" almost always found nothing and gave no clue why. It now
    /// searches several sensible roots and reports all of them — including the
    /// ones that do not exist — so the UI can say *where to put a plugin*.
    pub fn discover(&self) -> Discovery {
        // Canonicalise configured paths too: discovered paths are canonical
        // (symlinks like macOS's /var -> /private/var resolved), so comparing
        // raw strings would miss a plugin that is already loaded.
        let canon = |p: String| {
            std::fs::canonicalize(&p)
                .map(|c| c.display().to_string())
                .unwrap_or(p)
        };
        let configured: std::collections::HashSet<String> = self
            .config()
            .plugins
            .values()
            .map(|v| canon(v.path.clone()))
            .collect();

        let mut roots = Vec::new();
        // 1. The app's own plugin directory — the canonical place to drop files.
        roots.push((self.shared.plugins_dir.clone(), "app plugin directory"));
        // 2. Each configured plugin's directory (you already have plugins there).
        for e in self.config().plugins.values() {
            if let Some(dir) = self.resolve(&e.path).parent() {
                roots.push((dir.to_path_buf(), "configured plugin directory"));
            }
        }
        // 3. Sibling build outputs, so a freshly-built demo is discoverable.
        //    `<app-data>/../../wasm-plugin-host/target/wasm32-wasip1/release`
        if let Some(base) = self.shared.plugins_dir.parent() {
            for rel in [
                "../../../wasm-plugin-host/target/wasm32-wasip1/release",
                "../../wasm-plugin-host/target/wasm32-wasip1/release",
                "../wasm-plugin-host/target/wasm32-wasip1/release",
            ] {
                let p = base.join(rel);
                if p.exists() {
                    roots.push((p, "wasm-plugin-host build output"));
                }
            }
        }

        let mut found = Vec::new();
        let mut searched: Vec<SearchedRoot> = Vec::new();
        let mut seen: std::collections::HashSet<String> = Default::default();

        for (dir, label) in roots {
            // Normalise away `..` segments so the UI shows a real path.
            let dir = dir.canonicalize().unwrap_or(dir);
            let exists = dir.is_dir();
            let mut count = 0usize;
            if exists {
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    for entry in rd.flatten() {
                        let path = entry.path();
                        if path.extension().and_then(|e| e.to_str()) != Some("wasm") {
                            continue;
                        }
                        let path_str = path.display().to_string();
                        if configured.contains(&canon(path_str.clone()))
                            || !seen.insert(path_str.clone())
                        {
                            continue;
                        }
                        count += 1;
                        found.push(Discovered {
                            slot: path
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("plugin")
                                .to_string(),
                            path: path_str,
                        });
                    }
                }
            }
            searched.push(SearchedRoot {
                path: dir.display().to_string(),
                label: label.to_string(),
                exists,
                wasm_count: count,
            });
        }

        found.sort_by(|a, b| a.slot.cmp(&b.slot));
        Discovery { plugins: found, searched }
    }

    // -----------------------------------------------------------------------
    // Auto-reload watcher
    // -----------------------------------------------------------------------

    /// Start watching every enabled plugin's `.wasm` for changes and hot-reload
    /// on rebuild. Idempotent; a platform without a usable watcher is tolerated.
    pub fn start_watch(&self) {
        if self.watching() {
            return;
        }
        self.shared.watch_stop.store(false, Ordering::SeqCst);

        let shared = self.shared.clone();
        let fallback = std::time::Duration::from_millis(700);
        let handle = std::thread::spawn(move || {
            // mtime per slot, so we only reload what actually changed.
            let mut mtimes: std::collections::HashMap<String, u128> = Default::default();
            let mut watched_paths: Vec<PathBuf> = Vec::new();
            let mut watcher: Option<wasm_plugin_host::Watcher> = None;

            while !shared.watch_stop.load(Ordering::SeqCst) {
                // (Re)build the watcher if the set of files to watch changed.
                let paths: Vec<PathBuf> = {
                    let cfg = shared.config.lock().unwrap();
                    cfg.plugins
                        .iter()
                        .filter(|(_, e)| e.enabled && e.watch.unwrap_or(true))
                        .map(|(_, e)| {
                            let p = Path::new(&e.path);
                            if p.is_absolute() {
                                p.to_path_buf()
                            } else {
                                shared
                                    .config_path
                                    .parent()
                                    .unwrap_or_else(|| Path::new("."))
                                    .join(p)
                            }
                        })
                        .collect()
                };
                if paths != watched_paths {
                    watched_paths = paths.clone();
                    watcher = wasm_plugin_host::Watcher::new(&watched_paths).ok().flatten();
                    // Seed mtimes so an existing file is not reloaded immediately.
                    mtimes = watcher_mtimes(&watched_paths);
                }

                // Block for a change (or the fallback tick).
                let changed = match &watcher {
                    Some(w) => w.wait(fallback),
                    None => {
                        std::thread::sleep(fallback);
                        true
                    }
                };
                if !changed {
                    continue;
                }

                // Which watched files actually changed since last look?
                let now = watcher_mtimes(&watched_paths);
                let mut to_reload: Vec<String> = Vec::new();
                for (path, mtime) in &now {
                    let Some(slot) = slot_for_path(&shared.config, path) else {
                        continue;
                    };
                    if mtimes.get(&slot) != Some(mtime) {
                        mtimes.insert(slot.clone(), *mtime);
                        to_reload.push(slot);
                    }
                }
                if to_reload.is_empty() {
                    continue;
                }

                // Reload each changed slot on the async runtime. `block_on` from
                // a plain thread is fine (this is not a tokio worker).
                let studio = Studio {
                    shared: shared.clone(),
                };
                for slot in to_reload {
                    let studio = studio.clone();
                    let _ = tauri::async_runtime::block_on(async move {
                        studio.reload_slot(&slot).await
                    });
                }
            }
        });

        *self.shared.watch_handle.lock().unwrap() = Some(handle);
    }

    /// Stop the watcher and join its thread.
    pub fn stop_watch(&self) {
        self.shared.watch_stop.store(true, Ordering::SeqCst);
        let handle = self.shared.watch_handle.lock().unwrap().take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }

    // -----------------------------------------------------------------------
    // Shutdown
    // -----------------------------------------------------------------------

    /// Stop the watcher and dispose every fiber (slots first, then the bridge).
    ///
    /// Called on app exit; also handy in tests to prove nothing leaks a running
    /// fiber. Safe to call more than once.
    pub async fn shutdown(&self) {
        self.stop_watch();

        let slots: Vec<(String, FiberHandle)> =
            self.shared.mounted.lock().unwrap().drain(..).collect();
        for (_, fiber) in slots {
            fiber.dispose().await;
        }

        let bridge = self.shared.bridge.lock().unwrap().take();
        if let Some(fiber) = bridge {
            fiber.dispose().await;
        }
    }

    // -----------------------------------------------------------------------
    // Agents (the chat surface)
    // -----------------------------------------------------------------------

    /// The agent registry service, or an error if unavailable.
    fn agents(&self) -> Result<Arc<dsh_rs::api::services::AgentRegistryService>> {
        self.shared
            .ctx
            .require::<dsh_rs::api::services::AgentRegistryService>(dsh_rs::api::AGENTS_SERVICE)
            .map_err(|e| anyhow::anyhow!("agents service unavailable: {e}"))
    }

    /// One agent as a UI row.
    pub fn agent_row(&self, agent: &Arc<dyn dsh_rs::api::services::AgentView>) -> AgentRow {
        let session = agent.session();
        let messages = session.derive_messages();
        AgentRow {
            live: true,
            id: agent.id().to_string(),
            status: match agent.status() {
                dsh_rs::types::AgentStatus::Idle => "idle",
                dsh_rs::types::AgentStatus::Running => "running",
            }
            .to_string(),
            messages: messages.len(),
            turns: session.events().len(),
            busy: agent.driver_busy(),
            title: session_title(&messages),
            usage: session_usage(&session.events()),
        }
    }

    /// Every live agent.
    pub fn list_agents(&self) -> Vec<AgentRow> {
        self.live_agents()
            .iter()
            .map(|a| self.agent_row(a))
            .collect()
    }

    /// Every session the sidebar should show: live agents **first**, then the
    /// sessions on disk that are not currently loaded.
    ///
    /// This is the honest single list. A stored session that *is* live is
    /// reported once (as its live row), not twice — otherwise a user who
    /// restored a session would see two rows for it, one of them read-only.
    ///
    /// `live: false` rows are the read-only ones: openable, but with no driver
    /// behind them until `resume_session` builds one.
    pub fn list_sessions(&self) -> Vec<AgentRow> {
        let live = self.live_agents();
        let mut rows: Vec<AgentRow> = live.iter().map(|a| self.agent_row(a)).collect();
        let live_ids: std::collections::HashSet<String> =
            live.iter().map(|a| a.id().to_string()).collect();
        for mut row in self.stored_session_rows() {
            if live_ids.contains(&row.id) {
                continue;
            }
            row.live = false;
            rows.push(row);
        }
        rows
    }

    /// Rows for the sessions on disk, in the order the backend reports them.
    ///
    /// Each row is projected through the **same** `Session` type a live one uses
    /// (`Session::new` with the stored log as its seed), so a restored session
    /// shows exactly the title, turn count and usage it had while live. Deriving
    /// the counts by hand here would be a second implementation of dsh's surface
    /// projection, free to drift from the one the live rows use.
    ///
    /// Cached: `dsh` writes one line per stream chunk, so reading every session
    /// on every sidebar poll would be expensive. Invalidated by
    /// [`Studio::invalidate_stored_cache`] whenever this process writes.
    fn stored_session_rows(&self) -> Vec<AgentRow> {
        if let Some(cached) = self.shared.stored_cache.lock().unwrap().clone() {
            return cached;
        }
        let rows = self.scan_stored_sessions();
        *self.shared.stored_cache.lock().unwrap() = Some(rows.clone());
        rows
    }

    /// Drop the cached disk rows; the next `list_sessions` rescans.
    fn invalidate_stored_cache(&self) {
        *self.shared.stored_cache.lock().unwrap() = None;
    }

    fn scan_stored_sessions(&self) -> Vec<AgentRow> {
        let Some(backend) = self.persistence() else {
            return Vec::new();
        };
        // Newest first: the sessions a user is most likely to want are the ones
        // they were last in. `created_at` is not persisted (the JSONL backend
        // writes events, not the header), so the last event's timestamp is the
        // best available "last used" — and it is already in hand during the scan,
        // so ordering costs no second read of the file.
        let mut scanned: Vec<(u64, AgentRow)> = backend
            .list()
            .into_iter()
            .filter_map(|id| {
                let mut events = backend.load(&id).ok()?;
                // A session whose process was killed mid-turn has an open turn.
                // dsh's own reload path repairs it; project the repaired form so
                // the row matches what resuming it would show.
                dsh_rs::session::repair_crash_turns(&mut events);
                let last = events.iter().map(|e| e.time).max().unwrap_or(0);
                let session = dsh_rs::session::Session::new(
                    id.clone(),
                    dsh_rs::types::SessionHeader::default(),
                    events,
                );
                let messages = session.derive_messages();
                Some((
                    last,
                    AgentRow {
                        live: false,
                        id,
                        status: "stored".to_string(),
                        messages: messages.len(),
                        turns: session.events().len(),
                        busy: false,
                        title: session_title(&messages),
                        usage: session_usage(&session.events()),
                    },
                ))
            })
            .collect();
        scanned.sort_by_key(|(time, _)| std::cmp::Reverse(*time));
        scanned.into_iter().map(|(_, row)| row).collect()
    }

    /// The raw stored event log for a session — the persistence layer's view.
    ///
    /// Exposed so a test can assert on what is actually **on disk** (the thing
    /// that survives a restart) rather than on the in-memory session, which can
    /// look correct while the file is being rewritten with duplicates.
    pub fn stored_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<dsh_rs::types::SessionEvent>> {
        let backend = self
            .persistence()
            .ok_or_else(|| anyhow::anyhow!("no session store configured"))?;
        backend
            .load(session_id)
            .map_err(|e| anyhow::anyhow!("loading session `{session_id}`: {e}"))
    }

    /// The JSONL persistence backend, when a store directory is configured.
    fn persistence(
        &self,
    ) -> Option<Arc<dyn dsh_rs::api::services::SessionPersistenceApi>> {
        // The service is provided as `Arc<dyn SessionPersistenceApi>` (dsh's CLI
        // reads it the same way), so `get` yields an `Arc` of that `Arc`; the
        // inner one is what the callers want.
        self.shared
            .ctx
            .get::<Arc<dyn dsh_rs::api::services::SessionPersistenceApi>>(
                dsh_rs::api::SESSION_PERSISTENCE_SERVICE,
            )
            .map(|outer| (*outer).clone())
    }

    /// Load a stored session and put a live agent behind it, so the user can
    /// continue the conversation instead of only reading it.
    ///
    /// dsh has no `resume` entry point of its own — its CLI can *read* a
    /// transcript (`cmd_transcript`) but never continues one. The pieces it does
    /// provide are exactly what this needs, though: `CreateSessionOptions.seed`
    /// is documented as the "resume/fork/replay" hook, and it is what dsh's own
    /// `fork` uses. Seeding replays the stored **event log**, so the derived
    /// message history is restored without re-running any tool call.
    ///
    /// Idempotent: resuming an id that is already live returns that agent.
    pub fn resume_session(&self, session_id: &str) -> Result<String> {
        if let Some(agent) = self.shared.resumed.lock().unwrap().get(session_id) {
            return Ok(agent.id.to_string());
        }
        // Already live in the registry (created this run, not restored): nothing
        // to seed — the in-memory session is strictly newer than the file.
        if self.agents()?.get(session_id).is_some() {
            return Ok(session_id.to_string());
        }

        let backend = self
            .persistence()
            .ok_or_else(|| anyhow::anyhow!("no session store configured"))?;
        let mut events = backend
            .load(session_id)
            .map_err(|e| anyhow::anyhow!("loading session `{session_id}`: {e}"))?;
        if events.is_empty() {
            return Err(anyhow::anyhow!("session `{session_id}` has no events"));
        }
        // Close a turn left open by a crash, so the seed ends outside a turn.
        // dsh's `fork` *rejects* an open turn; repairing is the read path's
        // answer to the same condition, and it is what makes a killed-mid-turn
        // session resumable rather than permanently stuck.
        dsh_rs::session::repair_crash_turns(&mut events);

        // Resume keeps the session's own model configuration, recovered from the
        // log: every turn appends a `RequestHeader` carrying `LlmCallConfig`
        // (provider, model). Falling back to a default instead would silently
        // move a restored conversation onto a different model.
        let options = last_request_options(&events);

        // The working directory is **not** recoverable: dsh keeps `cwd` in the
        // `SessionHeader`, and the JSONL backend writes only events, never the
        // header. So a resumed session's tools run without a cwd. Passing a
        // guessed one (`$HOME`, the app-data dir) would be worse — it would
        // look like the original and quietly run tools somewhere else.
        let cwd = None;

        let sessions = self
            .shared
            .ctx
            .require::<dsh_rs::api::services::SessionService>(dsh_rs::api::SESSIONS_SERVICE)
            .map_err(|e| anyhow::anyhow!("sessions service unavailable: {e}"))?;

        let agent = in_runtime(|| -> Result<Arc<dsh_rs::core::Agent>> {
            let session = sessions.create(dsh_rs::types::CreateSessionOptions {
                id: Some(session_id.to_string()),
                cwd,
                seed: events,
                parent_session: None,
            });
            let agent = dsh_rs::core::Agent::new(
                session_id.to_string(),
                options,
                session,
                self.shared.ctx.clone(),
            );
            // Same spawn the registry uses, so a resumed agent's driver behaves
            // identically to a created one's.
            dsh_rs::core::loop_driver::spawn_driver(agent.clone());
            Ok(agent)
        })?;

        self.shared
            .resumed
            .lock()
            .unwrap()
            .insert(session_id.to_string(), agent);
        // It is live now, so it must not also be listed as a stored row.
        self.invalidate_stored_cache();
        Ok(session_id.to_string())
    }

    /// Create an agent. `provider`/`model` select the route; `cwd` is the tool
    /// working directory.
    pub fn create_agent(
        &self,
        id: Option<String>,
        provider: String,
        model: String,
        cwd: Option<String>,
    ) -> Result<String> {
        // Settings may have added this provider after boot; ensure the route exists.
        if provider != "mock" && !self.providers().iter().any(|p| p == &provider) {
            let _ = self.sync_llm_adapters();
        }
        if provider != "mock" && !self.providers().iter().any(|p| p == &provider) {
            anyhow::bail!(
                "LLM provider `{provider}` is not registered. Check base_url/api_key in settings, then retry."
            );
        }
        // Auto-minted ids must not collide with a session already on disk.
        // dsh's own `mint_id` is a process-local counter that restarts at 1, so
        // after a restart a "new" session would be handed the id of a restored
        // one — and since the JSONL backend **appends**, the new conversation
        // would be written into the old session's file. Two conversations in one
        // transcript, recoverable only by reading the timestamps.
        //
        // dsh's CLI does not hit this because every run is a new process with an
        // empty store directory in practice. The studio is long-lived and the
        // directory is its whole point, so it must pick ids around what exists.
        let id = Some(id.unwrap_or_else(|| self.new_session_id()));
        // `reg.create` spawns the agent's driver task with a bare `tokio::spawn`,
        // so a runtime context is mandatory here.
        in_runtime(|| {
            let reg = self.agents()?;
            let options = dsh_rs::types::AgentOptions {
                provider,
                model,
                max_tokens: None,
            };
            let agent = reg.create(id, options, cwd, None).map_err(anyhow::Error::msg)?;
            self.invalidate_stored_cache();
            Ok(agent.id().to_string())
        })
    }

    /// An id that no live agent and no stored session is using.
    ///
    /// Starts from a timestamp so the id also *reads* as distinct across runs
    /// (a bare counter would be legible but would look like a continuation of
    /// an older session); the counter only breaks ties within the same
    /// millisecond.
    fn new_session_id(&self) -> String {
        let taken = |cand: &str| -> bool {
            self.shared.resumed.lock().unwrap().contains_key(cand)
                || self
                    .agents()
                    .map(|r| r.get(cand).is_some())
                    .unwrap_or(false)
                || self.session_on_disk(cand)
        };
        loop {
            let n = self.shared.id_seq.fetch_add(1, Ordering::SeqCst);
            let ms = dsh_rs::types::now_ms();
            let cand = format!("session-{ms}-{n}");
            if !taken(&cand) {
                return cand;
            }
        }
    }

    /// Whether a session with this id is already stored on disk.
    fn session_on_disk(&self, id: &str) -> bool {
        self.persistence()
            .map(|b| b.list().iter().any(|s| s == id))
            .unwrap_or(false)
    }

    fn agent(&self, id: &str) -> Result<Arc<dyn dsh_rs::api::services::AgentView>> {
        // Resumed agents are held by us, not by the registry (see `Shared::resumed`).
        if let Some(agent) = self.shared.resumed.lock().unwrap().get(id) {
            return Ok(agent.clone() as Arc<dyn dsh_rs::api::services::AgentView>);
        }
        self.agents()?
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("no agent `{id}`"))
    }

    /// Every live agent — registry-created and resumed alike.
    ///
    /// The registry cannot hold a resumed agent (`AgentRegistry` has no
    /// `insert`), so a caller that only asked the registry would silently
    /// omit exactly the sessions the user restored.
    fn live_agents(&self) -> Vec<Arc<dyn dsh_rs::api::services::AgentView>> {
        let mut out: Vec<Arc<dyn dsh_rs::api::services::AgentView>> = self
            .agents()
            .map(|reg| reg.list().to_vec())
            .unwrap_or_default();
        for agent in self.shared.resumed.lock().unwrap().values() {
            out.push(agent.clone() as Arc<dyn dsh_rs::api::services::AgentView>);
        }
        out
    }

    /// Send a user message and wait for the turn to finish.
    ///
    /// While the agent is busy, a background task polls
    /// [`Self::transcript`] and emits `studio://chat-partial` events so the
    /// openhanako bridge (and Studio chat) can typewrite mid-generation
    /// instead of waiting for `when_idle`.
    pub async fn send_message(
        &self,
        agent_id: &str,
        text: String,
        msg_id: String,
    ) -> Result<()> {
        // Snapshot length *before* followup so the poller never treats the
        // previous assistant message as the new turn's stream seed.
        let baseline_len = self.transcript(agent_id).map(|m| m.len()).unwrap_or(0);

        let agent = self.agent(agent_id)?;
        agent.followup(dsh_rs::types::Message::user(
            msg_id,
            vec![text_block(text)],
        ));

        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let studio = self.clone();
        let id = agent_id.to_string();
        let poller = tauri::async_runtime::spawn(async move {
            let mut last_text = String::new();
            let mut last_reasoning = String::new();
            while !stop_flag.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                let Ok(rows) = studio.transcript(&id) else {
                    continue;
                };
                let Some(asst) = rows
                    .iter()
                    .skip(baseline_len)
                    .rev()
                    .find(|m| m.role == "assistant")
                else {
                    continue;
                };
                if asst.text == last_text && asst.reasoning == last_reasoning {
                    continue;
                }
                let text_delta = if asst.text.starts_with(&last_text) {
                    asst.text[last_text.len()..].to_string()
                } else {
                    String::new()
                };
                let reasoning_delta = if asst.reasoning.starts_with(&last_reasoning) {
                    asst.reasoning[last_reasoning.len()..].to_string()
                } else {
                    String::new()
                };
                last_text = asst.text.clone();
                last_reasoning = asst.reasoning.clone();
                let Some(app) = studio.shared.app.lock().unwrap().clone() else {
                    continue;
                };
                let payload = serde_json::json!({
                    "agentId": id,
                    "text": last_text,
                    "reasoning": last_reasoning,
                    "textDelta": text_delta,
                    "reasoningDelta": reasoning_delta,
                });
                let _ = app.emit("studio://chat-partial", payload);
            }
        });

        agent.when_idle().await;
        stop.store(true, Ordering::Relaxed);
        let _ = poller.await;

        self.flush_session(agent_id).await;
        // The session file just grew, so the cached disk rows are stale.
        self.invalidate_stored_cache();
        Ok(())
    }

    /// Checkpoint a session to its persistence backend.
    ///
    /// `attach_persistence` only registers a **write** backend, and that backend
    /// buffers in memory — `JsonlPersistence::on_event` pushes lines onto a
    /// `HashMap` and `flush()` is what writes them to the `.jsonl` file. Nothing
    /// calls it automatically, so without this the transcript never reaches the
    /// disk and the session does not survive a restart.
    ///
    /// This mirrors dsh's own CLI, which flushes explicitly after a turn
    /// (`main.rs`: `flush_session`). It is deliberate rather than a missing
    /// feature there: a checkpoint per turn is cheap, and flushing on every
    /// event would make the append path synchronous for every token.
    ///
    /// Best-effort: a failed checkpoint must not fail the turn the user just
    /// watched succeed. The events are still in memory for this process.
    async fn flush_session(&self, agent_id: &str) {
        let Ok(store) = self
            .shared
            .ctx
            .require::<dsh_rs::api::services::SessionService>(dsh_rs::api::SESSIONS_SERVICE)
        else {
            return;
        };
        if let Err(e) = store.flush(agent_id).await {
            eprintln!("[studio] flushing session `{agent_id}` failed: {e}");
        }
    }

    /// Queue a steer message (delivered at the next step boundary).
    pub fn steer(&self, agent_id: &str, text: String, msg_id: String) -> Result<()> {
        let agent = self.agent(agent_id)?;
        agent.steer(dsh_rs::types::Message::user(msg_id, vec![text_block(text)]));
        Ok(())
    }

    /// Cancel the agent's in-flight turn.
    pub fn cancel_agent(&self, agent_id: &str) -> Result<()> {
        let agent = self.agent(agent_id)?;
        agent.cancel(dsh_rs::types::AgentCancelCause::User, true);
        Ok(())
    }

    /// Dispose an agent (its session stays readable until disposed separately).
    pub fn dispose_agent(&self, agent_id: &str) -> Result<()> {
        // A resumed agent is not in the harness registry's map, so going through
        // the service wrapper would find nothing and return — leaving the driver
        // task running with no handle left to stop it (a leak per disposed
        // session). `AgentRegistry::dispose` cancels the agent and sets its
        // `disposed` flag *before* consulting the map, so it works here.
        let resumed = self.shared.resumed.lock().unwrap().remove(agent_id);
        in_runtime(|| {
            if let Some(agent) = resumed {
                self.shared.agent_owner.dispose(&agent);
                self.invalidate_stored_cache();
                return Ok(());
            }
            let reg = self.agents()?;
            if let Some(agent) = reg.get(agent_id) {
                reg.dispose(&agent);
            }
            Ok(())
        })
    }

    /// The full message history of an agent's session, mapped for the UI.
    pub fn transcript(&self, agent_id: &str) -> Result<Vec<ChatMessage>> {
        let agent = self.agent(agent_id)?;
        Ok(agent
            .session()
            .derive_messages()
            .iter()
            .map(chat_message)
            .collect())
    }

    // -----------------------------------------------------------------------
    // Plugin windows
    // -----------------------------------------------------------------------

    /// The window label for one of a plugin's declared windows.
    ///
    /// Namespaced by the slot so two plugins cannot collide, and stable so the
    /// window's *own* JS can look up what it should render.
    pub fn window_label(slot: &str, name: &str) -> String {
        let safe = |s: &str| {
            s.chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
                .collect::<String>()
        };
        format!("plugin-{}-{}", safe(slot), safe(name))
    }

    /// Every window a plugin declares, as the frontend needs it.
    ///
    /// Includes the derived `label`, so the UI can both open the window and,
    /// inside that window, discover what to render.
    pub fn plugin_windows(&self) -> Vec<PluginWindow> {
        let mut out = Vec::new();
        for (slot, ui) in self.shared.host.ui_decls() {
            for w in ui.windows {
                out.push(PluginWindow {
                    label: Self::window_label(&slot, &w.name),
                    slot: slot.clone(),
                    name: w.name,
                    component: w.component,
                    title: w.title.unwrap_or_else(|| slot.clone()),
                    width: w.width.unwrap_or(900.0),
                    height: w.height.unwrap_or(640.0),
                    open: match w.open {
                        wasm_plugin_host::WindowOpen::Auto => "auto",
                        wasm_plugin_host::WindowOpen::Manual => "manual",
                        wasm_plugin_host::WindowOpen::Startup => "startup",
                    }
                    .to_string(),
                    content: match w.content {
                        wasm_plugin_host::WindowContent::Html => "html",
                        wasm_plugin_host::WindowContent::App => "app",
                    }
                    .to_string(),
                });
            }
        }
        out
    }

    /// Look up one declared window by its (already derived) label.
    pub fn plugin_window_by_label(&self, label: &str) -> Option<PluginWindow> {
        self.plugin_windows().into_iter().find(|w| w.label == label)
    }

    /// Open a plugin window **with params**, and focus it if it already exists.
    ///
    /// This is the window-to-window communication primitive: the opener passes
    /// JSON, and the target window receives it as `window.__STUDIO_WINDOW__.params`
    /// (app windows) at document start, or via the `studio://window-params`
    /// event if it was already open.
    pub fn open_plugin_window_with(
        &self,
        label: &str,
        params: Option<serde_json::Value>,
    ) -> Result<()> {
        if let Some(p) = params {
            self.shared
                .pending_params
                .lock()
                .unwrap()
                .insert(label.to_string(), p);
        }
        self.open_plugin_window(label)
    }

    /// Send params to an **already-open** window (no-op if it is not open).
    ///
    /// A dashboard window can push updates to a detail window without
    /// reopening it.
    pub fn send_window_params(&self, label: &str, params: serde_json::Value) -> Result<()> {
        use tauri::{Emitter, Manager};
        let app = self
            .shared
            .app
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no app handle (headless mode)"))?;
        let win = app
            .get_webview_window(label)
            .ok_or_else(|| anyhow::anyhow!("window `{label}` is not open"))?;
        win.emit("studio://window-params", params)
            .map_err(|e| anyhow::anyhow!("emitting to `{label}`: {e}"))?;
        Ok(())
    }

    /// Close a window by label (so a plugin can dismiss its own windows).
    pub fn close_plugin_window(&self, label: &str) -> Result<()> {
        use tauri::Manager;
        let app = self
            .shared
            .app
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no app handle (headless mode)"))?;
        if let Some(win) = app.get_webview_window(label) {
            win.close()
                .map_err(|e| anyhow::anyhow!("closing `{label}`: {e}"))?;
        }
        self.shared.open_windows.lock().unwrap().remove(label);
        Ok(())
    }

    /// Open (or focus) a plugin's declared window.
    ///
    /// Two content modes:
    /// * `app` — loads the app; the window reads its own label and renders the
    ///   declared component full-window (the plugin's UI code is unchanged).
    /// * `html` — a self-contained page the plugin supplies. The host loads a
    ///   blank document and injects the plugin's `html` via an initialization
    ///   script, so the window is entirely the plugin's, with no app shell.
    pub fn open_plugin_window(&self, label: &str) -> Result<()> {
        use tauri::{Manager, WebviewWindowBuilder};

        let spec = self
            .plugin_window_by_label(label)
            .ok_or_else(|| anyhow::anyhow!("no plugin declares a window labelled `{label}`"))?;

        let app = self
            .shared
            .app
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no app handle (headless mode)"))?;

        // Already open? Just focus it — do not create a second one.
        if let Some(existing) = app.get_webview_window(label) {
            let _ = existing.set_focus();
            if let Some(params) = self.take_pending_params(label) {
                let _ = existing.emit("studio://window-params", params);
            }
            return Ok(());
        }

        let mut builder = WebviewWindowBuilder::new(&app, label, self.window_url(&spec))
            .title(&spec.title)
            .inner_size(spec.width, spec.height);

        // An `html` window gets its page injected at document start. The script
        // replaces the (blank) document with the plugin's own markup.
        if spec.content == "html" {
            if let Some(h) = self.window_html(label) {
                builder = builder.initialization_script(html_window_bootstrap(&h, label));
            }
        } else {
            // `app` windows receive their pending params as an init script too,
            // so they are available before the plugin's component mounts.
            if let Some(params) = self.peek_pending_params(label) {
                builder = builder
                    .initialization_script(app_window_bootstrap(label, &params.to_string()));
            }
        }

        let win = builder
            .build()
            .with_context(|| format!("building plugin window `{label}`"))?;
        let _ = win.set_focus();

        // Params were injected at document start; forget the pending copy.
        self.take_pending_params(label);

        self.shared
            .open_windows
            .lock()
            .unwrap()
            .insert(label.to_string(), spec.slot.clone());
        Ok(())
    }

    /// The URL a window loads. `app` windows load the SPA with their label in
    /// the query string (so a window can identify itself); `html` windows load
    /// a blank document that the init script replaces.
    fn window_url(&self, spec: &PluginWindow) -> tauri::WebviewUrl {
        match spec.content.as_str() {
            "html" => tauri::WebviewUrl::App("plugin-window-shell.html".into()),
            _ => tauri::WebviewUrl::App(Self::app_window_path(&spec.label).into()),
        }
    }

    /// The route an `app` window loads, as a path the SPA router can resolve.
    ///
    /// Split out (and public) because it is a **contract with the frontend**,
    /// not an implementation detail: the SPA routes on *pathname*, so this must
    /// name the route that renders a plugin component full-window. It used to
    /// be `index.html?plugin-window=<label>`, which silently did not work —
    /// `/index.html` matches the `[...plugin]` catch-all, so the plugin-window
    /// route never mounted and the window came up **blank**, while every
    /// headless test passed because they exercised the backend lookup, never
    /// the URL. Keeping it a plain function is what makes that testable.
    pub fn app_window_path(label: &str) -> String {
        format!("plugin-window?label={label}")
    }

    /// The plugin-supplied HTML for an `html` window, if declared.
    pub fn window_html(&self, label: &str) -> Option<String> {
        for (slot, ui) in self.shared.host.ui_decls() {
            for w in ui.windows {
                if Self::window_label(&slot, &w.name) == label {
                    return w.html;
                }
            }
        }
        None
    }

    /// Queue params for a window, to be delivered when it opens (or focused).
    pub fn queue_window_params(&self, label: &str, params: serde_json::Value) {
        self.shared
            .pending_params
            .lock()
            .unwrap()
            .insert(label.to_string(), params);
    }

    /// Params queued for a window that has not opened yet.
    pub fn peek_pending_params(&self, label: &str) -> Option<serde_json::Value> {
        self.shared.pending_params.lock().unwrap().get(label).cloned()
    }

    pub fn take_pending_params(&self, label: &str) -> Option<serde_json::Value> {
        self.shared.pending_params.lock().unwrap().remove(label)
    }

    /// Close every window owned by `slot` (called when it stops).
    pub fn close_plugin_windows(&self, slot: &str) {
        use tauri::Manager;
        let Some(app) = self.shared.app.lock().unwrap().clone() else {
            return;
        };
        let labels: Vec<String> = {
            let mut map = self.shared.open_windows.lock().unwrap();
            let mine: Vec<String> = map
                .iter()
                .filter(|(_, owner)| owner.as_str() == slot)
                .map(|(l, _)| l.clone())
                .collect();
            for l in &mine {
                map.remove(l);
            }
            mine
        };
        for label in labels {
            if let Some(win) = app.get_webview_window(&label) {
                let _ = win.close();
            }
        }
    }

    /// Open every `auto` window belonging to an active slot.
    fn open_auto_windows(&self, slot: &str) {
        for w in self.plugin_windows() {
            if w.slot == slot && w.open == "auto" {
                self.request_window(&w.label);
            }
        }
    }

    /// Record that `label` should be open, and open it if we already can.
    ///
    /// During boot we cannot: the app handle is installed *after* plugins
    /// autoload. Recording intent and replaying it later (see
    /// [`apply_startup_windows`](Self::apply_startup_windows)) is what makes an
    /// `auto` window actually appear at launch instead of failing silently.
    fn request_window(&self, label: &str) {
        {
            let mut wanted = self.shared.wanted_windows.lock().unwrap();
            if !wanted.iter().any(|l| l == label) {
                wanted.push(label.to_string());
            }
        }
        self.drain_wanted_windows();
    }

    /// Open every requested-but-not-yet-serviced window, now that a handle may
    /// exist. A no-op without an app handle, so the intent survives until boot
    /// can replay it (this is also what makes the request observable in tests).
    fn drain_wanted_windows(&self) {
        if self.shared.app.lock().unwrap().is_none() {
            return;
        }
        let labels: Vec<String> = std::mem::take(&mut *self.shared.wanted_windows.lock().unwrap());
        for label in labels {
            if let Err(e) = self.open_plugin_window(&label) {
                eprintln!("[studio] opening requested window `{label}` failed: {e}");
            }
        }
    }

    /// Windows plugins asked to open at mount time that have not been opened
    /// yet. Non-empty after a headless mount; drained once a real app handle
    /// exists. Exposed so the startup path can be tested without a window.
    pub fn windows_wanted_at_mount(&self) -> Vec<String> {
        self.shared.wanted_windows.lock().unwrap().clone()
    }

    /// The window the app should open **at launch, instead of its own default
    /// view**, if a plugin claims it.
    ///
    /// `Ok(None)` means no plugin asks for it — show the app's normal window.
    /// Two claimants is an error rather than a coin flip (see
    /// `WindowOpen::Startup`): "which window starts" has exactly one answer.
    pub fn startup_window(&self) -> Result<Option<PluginWindow>> {
        let mut found: Vec<PluginWindow> = self
            .plugin_windows()
            .into_iter()
            .filter(|w| w.open == "startup")
            .collect();
        match found.len() {
            0 => Ok(None),
            1 => Ok(Some(found.remove(0))),
            n => Err(anyhow::anyhow!(
                "{n} plugins declare a startup window ({}); only one may own the \
                 launch view — the winner would otherwise depend on load order",
                found
                    .iter()
                    .map(|w| w.label.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// Decide which window the user actually sees at launch.
    ///
    /// Called once from [`Studio::boot`], after the app handle exists — the
    /// point where window requests that were queued during autoload can finally
    /// become windows. The app's own window starts hidden (see
    /// `tauri.conf.json`), so a plugin that claims the launch view never lets
    /// the default page flash first.
    ///
    /// **It always ends with a window on screen.** A startup window that fails
    /// to open falls back to the app's own window rather than leaving the user
    /// with nothing.
    pub fn apply_startup_windows(&self, app: &tauri::AppHandle) {
        use tauri::Manager;

        let mut main_hidden = false;
        match self.startup_window() {
            Ok(Some(sw)) => match self.open_plugin_window(&sw.label) {
                Ok(()) => {
                    if let Some(main) = app.get_webview_window(MAIN_LABEL) {
                        let _ = main.hide();
                        main_hidden = true;
                    }
                }
                Err(e) => eprintln!(
                    "[studio] startup window `{}` failed to open ({e}); \
                     falling back to the app window",
                    sw.label
                ),
            },
            Ok(None) => {}
            Err(e) => eprintln!("[studio] {e}"),
        }

        // No plugin claimed the launch view, or the claim could not be honoured:
        // the app's own window is the view.
        if !main_hidden {
            if let Some(main) = app.get_webview_window(MAIN_LABEL) {
                let _ = main.show();
            }
        }

        // `auto` windows a plugin asked for at mount time are still queued.
        self.drain_wanted_windows();
    }

    // -----------------------------------------------------------------------
    // Catalog — every plugin the app knows about
    // -----------------------------------------------------------------------

    /// Build the full plugin catalog: everything **configured** plus everything
    /// **discoverable on disk**, with a running status for each.
    ///
    /// This is what the Plugins page lists. It deliberately includes plugins
    /// that are *not* running, so the UI can show "available but not started"
    /// rather than hiding them — and so a plugin never disappears when it is
    /// stopped.
    pub fn catalog(&self) -> Vec<CatalogEntry> {
        use std::collections::BTreeMap;

        // Running plugins, keyed by slot.
        let running: BTreeMap<String, (String, String, usize, bool)> = self
            .shared
            .host
            .list_plugins()
            .into_iter()
            .map(|(slot, plugin, state, tools, active)| {
                (slot, (plugin, format!("{state:?}").to_lowercase(), tools, active))
            })
            .collect();

        let mut out: Vec<CatalogEntry> = Vec::new();

        // 1. Everything in the config (running or not).
        let cfg = self.config();
        for (slot, entry) in &cfg.plugins {
            let path = self.resolve(&entry.path);
            let path_s = path.display().to_string();
            let canon = canon_path(&path_s);
            let run = running.get(slot);
            out.push(CatalogEntry {
                slot: slot.clone(),
                plugin: run.map(|r| r.0.clone()).unwrap_or_else(|| slot.clone()),
                path: path_s,
                running: run.is_some(),
                active: run.map(|r| r.3).unwrap_or(false),
                state: run
                    .map(|r| r.1.clone())
                    .unwrap_or_else(|| "stopped".to_string()),
                tool_count: run.map(|r| r.2).unwrap_or(0),
                in_config: true,
                enabled: entry.enabled,
                exists: path.exists(),
                _canon: canon,
            });
        }

        // 2. Anything discoverable on disk that is not already configured.
        for q in self.discover().plugins {
            if out.iter().any(|e| canon_path(&e.path) == canon_path(&q.path)) {
                continue;
            }
            out.push(CatalogEntry {
                slot: q.slot.clone(),
                plugin: q.slot,
                path: q.path,
                running: false,
                active: false,
                state: "available".to_string(),
                tool_count: 0,
                in_config: false,
                enabled: false,
                exists: true,
                _canon: String::new(),
            });
        }

        out.sort_by(|a, b| a.slot.cmp(&b.slot));
        out
    }

    // -----------------------------------------------------------------------
    // Status
    // -----------------------------------------------------------------------

    pub fn status(&self) -> StudioStatus {
        let plugins = self.shared.host.list_plugins();
        let wasm_tool_count = self.shared.host.list_tools().len();
        let tool_count = self
            .shared
            .ctx
            .require::<dsh_rs::api::services::ToolsService>(dsh_rs::api::TOOLS_SERVICE)
            .map(|t| t.list().len())
            .unwrap_or(wasm_tool_count);
        StudioStatus {
            booted: self.shared.booted,
            plugins_dir: self.shared.plugins_dir.display().to_string(),
            config_path: self.shared.config_path.display().to_string(),
            slot_count: plugins.len(),
            wasm_tool_count,
            tool_count,
            service_count: self.shared.host.services().len(),
            watching: self.watching(),
            providers: self.providers(),
        }
    }

    /// Registered LLM route names.
    ///
    /// `mock` is always available — dsh's base bundle registers it — but it is
    /// only *reported* here if the service says so, rather than being asserted
    /// by us. If the LLM seam is missing entirely the list is empty, and the UI
    /// renders "no provider"; inventing one would hide a broken boot.
    pub fn providers(&self) -> Vec<String> {
        self.shared
            .ctx
            .require::<dsh_rs::api::services::LlmService>(dsh_rs::api::LLM_SERVICE)
            // `id` is the **route** the user configured (`deepseek`); `name` is
            // the adapter's display name, which is the same string
            // (`openai-compatible`) for every OpenAI-shaped endpoint. Taking
            // `name` would report one indistinguishable entry per configured
            // provider — and the shell passes this value to `create_agent` as
            // the provider route, so it has to be the id.
            .map(|llm| llm.list_providers().into_iter().map(|p| p.id).collect())
            .unwrap_or_default()
    }
}

/// One `.wasm` found on disk that is not yet configured.
#[derive(Debug, Clone, Serialize)]
pub struct Discovered {
    pub slot: String,
    pub path: String,
}

/// The result of a discovery scan: what was found, and where we looked.
#[derive(Debug, Clone, Serialize)]
pub struct Discovery {
    pub plugins: Vec<Discovered>,
    /// Every directory consulted, so the UI can explain an empty result.
    pub searched: Vec<SearchedRoot>,
}

/// One directory consulted during discovery.
#[derive(Debug, Clone, Serialize)]
pub struct SearchedRoot {
    pub path: String,
    pub label: String,
    pub exists: bool,
    /// How many *new* `.wasm` files were found here.
    pub wasm_count: usize,
}

fn slot_for_path(cfg: &Mutex<Config>, path: &str) -> Option<String> {
    let p = Path::new(path);
    let cfg = cfg.lock().unwrap();
    cfg.plugins
        .iter()
        .find(|(_, e)| {
            let ep = Path::new(&e.path);
            ep == p || ep.file_name() == p.file_name()
        })
        .map(|(k, _)| k.clone())
}

/// `path -> mtime_ns` for every path that currently exists.
fn watcher_mtimes(paths: &[PathBuf]) -> std::collections::HashMap<String, u128> {
    let mut out = std::collections::HashMap::new();
    for p in paths {
        if let Ok(md) = std::fs::metadata(p) {
            if let Ok(t) = md.modified() {
                if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
                    out.insert(p.display().to_string(), d.as_nanos());
                }
            }
        }
    }
    out
}

/// Run `f` on the blocking pool and await it.
///
/// **Required for every call that reaches a guest.** `wasmtime-wasi` 44
/// implements WASI p1 through `runtime::in_tokio`, which does
/// `Handle::current().block_on(..)`. Invoking that from an async context nests
/// `block_on` and panics ("Cannot start a runtime from within a runtime"), so a
/// guest-touching call must never run on a tokio worker thread.
///
/// If there is no runtime at all (a synchronous caller, or a test), `f` runs
/// inline — that path is already safe.
async fn off_runtime<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::spawn_blocking(f)
            .await
            .expect("blocking task panicked")
    } else {
        f()
    }
}

/// Run `f` with a tokio runtime context available.
///
/// dsh's agent driver (`loop_driver::spawn_driver`) and cordis's
/// fire-and-forget event dispatch both call a **bare `tokio::spawn`**, which
/// panics with "there is no reactor running" outside a runtime. A
/// *synchronous* Tauri command runs without one — so any studio method that can
/// reach those paths must guarantee a context.
///
/// The `try_current` check matters: `block_on` panics if called from inside a
/// runtime, so we only ever take that branch when there is none. The tasks
/// spawned under Tauri's global runtime outlive this call.
fn in_runtime<R>(f: impl FnOnce() -> R) -> R {
    if tokio::runtime::Handle::try_current().is_ok() {
        f()
    } else {
        tauri::async_runtime::block_on(async { f() })
    }
}

/// How long a fiber may take to reach a steady state before we stop waiting.
///
/// Generous: this is a safety net, not a latency budget.
const SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// How often to re-check while waiting.
const SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// Wait for a fiber to settle, without trusting the settle notification.
///
/// **Why not `Fiber::join()`.** We hit an intermittent hang (roughly 1 in 10
/// fresh processes) where boot stopped after `[bundle] joining manifest` and
/// never advanced. `join` waits on a `watch` channel that is bumped at each
/// transition, and it reads the busy flag *before* registering its snapshot —
/// a window in which a transition can be missed. We could **not** reproduce that
/// as a minimal case (2000 iterations of a plugin settling while joined: zero
/// timeouts), so the race is a *suspicion*, not an established root cause.
///
/// So this helper deliberately does not depend on that diagnosis. `state()` is a
/// lock-free read of an atomic mirror that the fiber republishes at every
/// transition, and we simply poll it. Polling cannot miss a transition the way
/// a notification can: correctness rests on the current state, not on having
/// observed every change.
///
/// A timeout is **not** a failure. A fiber that is still `Pending` because a
/// service is not up yet is a legitimate state the studio already supports, so
/// we log and continue. Only a genuinely `Failed` fiber is an error.
async fn join_bounded(
    fiber: &cordis::fiber::FiberHandle,
    timeout: std::time::Duration,
    what: &str,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match fiber.state() {
            // Both settled states are fine to proceed with:
            //   Active  — effects are live;
            //   Pending — waiting on a service, a supported state.
            //
            // `Pending` must return at once, not fall through to the wait: an
            // unsatisfied inject stays Pending for as long as its provider is
            // absent, so waiting would burn the whole budget on every such
            // mount. (Measured: 10.03s per mount before this branch existed.)
            cordis::fiber::FiberState::Active | cordis::fiber::FiberState::Pending => {
                return Ok(())
            }
            cordis::fiber::FiberState::Failed => {
                return Err(anyhow::anyhow!("{what} fiber failed to start"));
            }
            // Loading / Unloading / Disposed: keep waiting.
            cordis::fiber::FiberState::Loading
            | cordis::fiber::FiberState::Unloading
            | cordis::fiber::FiberState::Disposed => {}
        }
        if tokio::time::Instant::now() >= deadline {
            eprintln!(
                "[studio] {what} did not settle within {timeout:?} (state {:?}); continuing",
                fiber.state()
            );
            return Ok(());
        }
        tokio::time::sleep(SETTLE_POLL).await;
    }
}

/// Install the dsh base bundle on `ctx`.
///
/// **Async on purpose.** cordis drives each plugin fiber with a tokio task, so
/// the runtime this runs on must outlive the studio. Tauri owns a global runtime
/// for exactly this reason; `Studio::boot` blocks on it, while tests call
/// [`Studio::with_hook`] inside their own `#[tokio::test]`.
async fn boot_harness(ctx: &cordis::Context, config: dsh_rs::bundle::BaseConfig) -> Result<()> {
    dsh_rs::bundle::install_base(ctx, config)
        .await
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("dsh base bundle: {e}"))
}

/// Translate the studio's own config into dsh's [`BaseConfig`].
///
/// This is the single place the studio config meets the harness's, so the two
/// things we take from dsh rather than re-implementing are both decided here
/// and nowhere else:
///
/// * **`store_dir`** — session persistence. Derived from the app-data dir so it
///   sits beside `studio.json` and the plugin cache, and created on demand (dsh
///   writes into it; a missing directory would fail the first append).
/// * **`adapters`** — the provider routes. `studio.json` keeps them under
///   `extra.llm.providers` as `{ name: { base_url, api_key, model } }`, which is
///   exactly the shape dsh's llm plugin reads under `adapters`; this copies it
///   across unchanged rather than reserialising, so a section dsh understands
///   and we do not still reaches it.

/// Default / configurable system-prompt persona for Studio agents.
///
/// Without this, OpenAI-compatible gateways (notably 9router) often ship their
/// own coding-agent system prompt, so "你是谁" yields a terse coding persona
/// and subsequent turns keep reinforcing it via session history.
fn install_studio_persona(ctx: &cordis::Context, config: &Config) {
    let Ok(prompt) = ctx.require::<dsh_rs::api::services::SystemPromptService>(
        dsh_rs::api::SYSTEM_PROMPT_SERVICE,
    ) else {
        return;
    };
    let custom = config
        .extra
        .as_ref()
        .and_then(|e| e.get("llm"))
        .and_then(|llm| llm.get("system_prompt"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let default = concat!(
        "你是 Studio 里的助手。用用户正在用的语言简洁回答。\n\n",
        "你可以读写文件、搜索代码、执行命令来完成任务；但用户只是闲聊、打招呼或问你是谁时，",
        "正常对话即可。不要自称「极简编程代理」，也不要主动要求用户「给任务」。",
    );
    let text = custom.unwrap_or(default);
    prompt.section("studio-persona", 0, text, false);
}

fn base_config(config: &Config, app_data_dir: &Path) -> dsh_rs::bundle::BaseConfig {
    let store_dir = app_data_dir.join("sessions");
    let _ = std::fs::create_dir_all(&store_dir);

    // dsh's llm plugin always registers a built-in `mock` route first, then
    // mounts whatever we put in `adapters`. If studio.json still has a
    // display-only `mock: {}` (or any entry without base_url), install_base
    // dies with ROUTE_CONFLICT / a useless empty adapter. Strip those here.
    let adapters = config
        .extra
        .as_ref()
        .and_then(|e| e.get("llm"))
        .and_then(|llm| llm.get("providers"))
        .and_then(|p| p.as_object())
        .map(|providers| {
            let mut out = serde_json::Map::new();
            for (name, entry) in providers {
                if name == "mock" {
                    continue;
                }
                let Some(obj) = entry.as_object() else { continue };
                let base_url = obj
                    .get("base_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if base_url.is_empty() {
                    continue;
                }
                out.insert(name.clone(), entry.clone());
            }
            serde_json::Value::Object(out)
        })
        .filter(|p| p.as_object().is_some_and(|m| !m.is_empty()));

    dsh_rs::bundle::BaseConfig {
        store_dir: Some(store_dir),
        adapters,
        ..Default::default()
    }
}

/// The dsh services a WASM plugin is allowed to `inject` across the boundary.
pub(crate) fn dsh_services() -> Vec<&'static str> {
    vec![
        dsh_rs::api::TOOLS_SERVICE,
        dsh_rs::api::SESSIONS_SERVICE,
        dsh_rs::api::LLM_SERVICE,
        dsh_rs::api::AGENTS_SERVICE,
        dsh_rs::api::SYSTEM_PROMPT_SERVICE,
        dsh_rs::api::MANIFEST_SERVICE,
    ]
}

/// The app-data directory: `<app-data>`, created on demand.
fn app_data_dir(app: &AppHandle) -> PathBuf {
    use tauri::Manager;
    let base = app
        .path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("dsh-wasm-studio"));
    let _ = std::fs::create_dir_all(&base);
    base
}

/// A one-line summary of studio state, for the dashboard header.
#[derive(Debug, Serialize)]
pub struct StudioStatus {
    pub booted: bool,
    pub plugins_dir: String,
    pub config_path: String,
    pub slot_count: usize,
    /// Tools contributed by WASM plugins.
    pub wasm_tool_count: usize,
    /// Total tools on the harness registry (WASM + dsh built-ins).
    pub tool_count: usize,
    pub service_count: usize,
    /// Whether the auto-reload watcher is running.
    pub watching: bool,
    /// Registered LLM route names (`mock`, plus anything from `extra.llm`).
    ///
    /// The UI needs these to pick a provider instead of hardcoding `mock`:
    /// a configured model that the chat silently ignores is the kind of bug
    /// that looks like "the model is bad".
    pub providers: Vec<String>,
}


// ---------------------------------------------------------------------------
// Chat surface types
// ---------------------------------------------------------------------------

/// One agent as the UI sees it.
#[derive(Debug, Clone, Serialize)]
pub struct AgentRow {
    pub id: String,
    /// `idle` | `running` | `stored`. `stored` means the session is on disk but
    /// has no driver behind it yet — opening it is read-only until resumed.
    pub status: String,
    /// Whether a driver is behind this row right now. A stored row is readable
    /// but cannot be sent to; the UI must know the difference rather than
    /// offering a composer that will fail.
    pub live: bool,
    pub messages: usize,
    /// Number of session events (turns/steps/tool calls) — a rough activity meter.
    pub turns: usize,
    pub busy: bool,
    /// A human-readable label for lists: the first user message, truncated.
    ///
    /// Derived rather than stored, because the agent's own id is a generated
    /// string and a list of those is unreadable. Empty until the first message
    /// — the UI shows a placeholder for that case.
    pub title: String,
    /// Cumulative token usage across the session, when the provider reported it.
    pub usage: TokenUsageRow,
}

/// Cumulative per-session token accounting.
///
/// `None` fields mean "the provider did not report this", which is why they are
/// separate from a `0`: a provider without cache support should not look like
/// one that reported a cache miss.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TokenUsageRow {
    pub input: u64,
    pub output: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    /// Number of assistant messages that carried usage — i.e. how many calls
    /// these totals span. Without it, "1234 tokens" cannot be read as a rate.
    pub calls: usize,
}

/// One message in a chat transcript.
#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    /// `user` | `assistant` | `system`.
    pub role: String,
    /// Visible text (concatenated text blocks).
    pub text: String,
    /// Reasoning blocks, kept separate so the UI can collapse them.
    pub reasoning: String,
    /// Tool calls the assistant requested.
    pub tool_calls: Vec<ChatToolCall>,
    /// Tool results delivered back to the model.
    pub tool_results: Vec<ChatToolResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON string as the model produced it.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatToolResult {
    pub tool_call_id: String,
    pub content: String,
    pub is_error: bool,
}

/// A text content block.
fn text_block(text: impl Into<String>) -> dsh_rs::types::ContentBlock {
    dsh_rs::types::ContentBlock::Text { text: text.into() }
}

/// The model configuration a stored session was last using.
///
/// Every turn appends a `RequestHeader` carrying the full `LlmCallConfig`, so the
/// log is its own record of which provider and model the conversation ran on.
/// Resuming with a *default* instead would silently move a restored conversation
/// onto a different model — the events would say one thing and the next request
/// another.
///
/// Falls back to the mock route only when the log has no header at all (an empty
/// or hand-truncated session). `mock` is dsh's own default and is always
/// registered, so the agent is still constructible; it will refuse real work,
/// which is the honest outcome for a log that does not say what it used.
fn last_request_options(events: &[dsh_rs::types::SessionEvent]) -> dsh_rs::types::AgentOptions {
    use dsh_rs::types::SessionEventData;
    let config = events.iter().rev().find_map(|e| match &e.data {
        SessionEventData::RequestHeader { header } => Some(&header.config),
        _ => None,
    });
    match config {
        Some(c) => dsh_rs::types::AgentOptions {
            provider: c.provider.clone(),
            model: c.model.clone(),
            max_tokens: c.max_tokens,
        },
        None => dsh_rs::types::AgentOptions {
            provider: "mock".to_string(),
            model: "mock-1".to_string(),
            max_tokens: None,
        },
    }
}

/// A readable label for an agent, derived from its first user message.
///
/// An agent's own id is a generated string ("agent-7f3a…"), which makes a
/// session list unreadable. The first thing the user typed is the label they
/// will recognise, and it needs no extra storage — it is already in the
/// transcript. Truncated **by characters, not bytes**, so a multi-byte title
/// cannot be cut mid-codepoint.
fn session_title(messages: &[dsh_rs::types::Message]) -> String {
    use dsh_rs::types::{ContentBlock, Role};
    let Some(first) = messages.iter().find(|m| m.role == Role::User) else {
        return String::new();
    };
    let mut text = String::new();
    for block in &first.content {
        if let ContentBlock::Text { text: t } = block {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(t.trim());
        }
    }
    let text = text.trim();
    const MAX: usize = 60;
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX).collect();
    out.push('…');
    out
}

/// Cumulative token usage across a session's assistant messages.
///
/// Read from the session events rather than tracked separately, because dsh
/// already attaches [`TokenUsage`](dsh_rs::types::TokenUsage) to each
/// `AssistantMessage` event — the numbers exist, they were just not reachable
/// from the UI. Summing them here means no second source of truth to drift.
///
/// `cache_*` and `reasoning` stay `None` unless **every** contributing call
/// reported them: a partial sum would understate the total while looking
/// exact.
fn session_usage(events: &[dsh_rs::types::SessionEvent]) -> TokenUsageRow {
    use dsh_rs::types::SessionEventData;
    let mut row = TokenUsageRow::default();
    let mut cache_read = Some(0u64);
    let mut cache_write = Some(0u64);
    let mut reasoning = Some(0u64);

    for event in events {
        let SessionEventData::AssistantMessage { usage: Some(u), .. } = &event.data else {
            continue;
        };
        row.input += u.input_tokens;
        row.output += u.output_tokens;
        row.calls += 1;
        match (u.cache_read_tokens, &mut cache_read) {
            (Some(v), Some(acc)) => *acc += v,
            (None, acc) => *acc = None,
            _ => {}
        }
        match (u.cache_write_tokens, &mut cache_write) {
            (Some(v), Some(acc)) => *acc += v,
            (None, acc) => *acc = None,
            _ => {}
        }
        match (u.reasoning_tokens, &mut reasoning) {
            (Some(v), Some(acc)) => *acc += v,
            (None, acc) => *acc = None,
            _ => {}
        }
    }

    // A session with no reported usage at all should not claim zero cache use.
    if row.calls > 0 {
        row.cache_read = cache_read;
        row.cache_write = cache_write;
        row.reasoning = reasoning;
    }
    row
}

/// Map a dsh `Message` onto the UI transcript shape.
fn chat_message(m: &dsh_rs::types::Message) -> ChatMessage {
    use dsh_rs::types::ContentBlock;
    let role = match m.role {
        dsh_rs::types::Role::User => "user",
        dsh_rs::types::Role::Assistant => "assistant",
        dsh_rs::types::Role::System => "system",
    }
    .to_string();

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let mut tool_results = Vec::new();

    for block in &m.content {
        match block {
            ContentBlock::Text { text: t } => text.push_str(t),
            ContentBlock::Reasoning { text: t } => reasoning.push_str(t),
            ContentBlock::ToolCall { id, name, arguments } => tool_calls.push(ChatToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => {
                let inner: String = content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .collect::<Vec<_>>()
                    .join("");
                tool_results.push(ChatToolResult {
                    tool_call_id: tool_call_id.clone(),
                    content: inner,
                    is_error: is_error.unwrap_or(false),
                });
            }
        }
    }

    ChatMessage {
        role,
        text,
        reasoning,
        tool_calls,
        tool_results,
    }
}

/// Register model providers described by the config's optional `llm` section.
///
/// Shape (all optional except `base_url` for a real provider):
///
/// ```json
/// { "llm": {
///     "providers": {
///       "deepseek": { "base_url": "https://api.deepseek.com/v1",
///                     "api_key": "sk-...", "model": "deepseek-chat" }
///     },
///     "default": "mock"
/// }}
/// ```
///
/// The `mock` route is always available (dsh registers it), so an app with no
/// provider configured can still exercise tool calls.
/// One row of the plugin catalog: known, and whether it is running.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogEntry {
    pub slot: String,
    pub plugin: String,
    pub path: String,
    /// Is a guest instance loaded right now?
    pub running: bool,
    /// Is it *active* (injects satisfied)? `false` for a running-but-pending plugin.
    pub active: bool,
    /// `active` | `pending` | `stopped` | `available` | ...
    pub state: String,
    pub tool_count: usize,
    /// Is it part of the persisted config?
    pub in_config: bool,
    pub enabled: bool,
    /// Does the `.wasm` file actually exist?
    pub exists: bool,
    #[serde(skip)]
    pub _canon: String,
}

/// Canonical form of a path, or the path unchanged if it cannot be resolved.
fn canon_path(p: &str) -> String {
    std::fs::canonicalize(p)
        .map(|c| c.display().to_string())
        .unwrap_or_else(|_| p.to_string())
}


/// One window a plugin declares, with its derived label.
#[derive(Debug, Clone, Serialize)]
pub struct PluginWindow {
    /// The real Tauri window label (namespaced by the owning slot).
    pub label: String,
    /// The plugin slot that declares it.
    pub slot: String,
    /// The plugin-local name from the declaration.
    pub name: String,
    /// Which registered component to render in the window.
    pub component: String,
    pub title: String,
    pub width: f64,
    pub height: f64,
    /// `auto` (host opens it when the plugin activates) or `manual`.
    pub open: String,
    /// `app` (render a registered component) or `html` (a self-contained page).
    pub content: String,
}


/// The bootstrap script for an `html` window: replace the document with the
/// plugin's markup at document-start, so nothing of the shell ever paints.
///
/// A `<base>`-less replacement is Deliberately blunt — the plugin owns the page.
pub fn html_window_bootstrap(html: &str, label: &str) -> String {
    // JSON-encode so quotes/newlines in the HTML cannot break out of the string.
    let payload = serde_json::to_string(html).unwrap_or_else(|_| "\"\"".to_string());
    let label_json = serde_json::to_string(label).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "((html, label) => {{
           const write = () => {{
             document.open();
             document.write('<!doctype html><html><head><meta charset=\"utf-8\"></head><body>' + html + '</body></html>');
             document.close();
             window.__STUDIO_WINDOW__ = {{ label }};
           }};
           if (document.readyState === 'loading') {{
             document.addEventListener('DOMContentLoaded', write, {{ once: true }});
             // document.write must happen before parsing ends; do it now.
             write();
           }} else {{ write(); }}
         }})({payload}, {label_json});"
    )
}

/// The bootstrap for an `app` window: make the window's params available to the
/// plugin's component before it mounts.
pub fn app_window_bootstrap(label: &str, params_json: &str) -> String {
    let label_json = serde_json::to_string(label).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "window.__STUDIO_WINDOW__ = {{ label: {label_json}, params: {params_json} }};"
    )
}
