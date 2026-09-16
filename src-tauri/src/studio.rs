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
//! ## Persistence
//!
//! Desired state lives in `<app-data>/studio.json`, in the *host's own*
//! [`wasm_plugin_host::Config`] format — so the file is interoperable with the
//! host CLI's supervisor, not a bespoke schema. Enabled plugins are loaded on
//! boot; every mutation writes the file back.

use std::collections::BTreeMap;
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
    /// slot -> its cordis fiber (the slot's own plugin instance).
    mounted: Mutex<Vec<(String, FiberHandle)>>,
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
        boot_harness(&ctx).await?;

        // Register any model provider from config. Without one the harness can
        // only run the `mock` route, which is still enough to test tool calls.
        register_llm_providers(&ctx, &config);

        // Tell the WASM registry which dsh services WASM plugins may inject.
        for svc in dsh_services() {
            host.declare_dsh_service(svc);
        }

        // Mount the shared flow bridge once (it is a host-wide concern).
        let bridge_plugin: Arc<dyn cordis::plugin::Plugin> =
            Arc::new(FlowBridgePlugin::new(host.clone()));
        let bridge = ctx.plugin(bridge_plugin, None);
        bridge
            .join()
            .await
            .map_err(|e| anyhow::anyhow!("flow bridge failed to converge: {e}"))?;

        let studio = Studio {
            shared: Arc::new(Shared {
                ctx,
                host,
                mounted: Mutex::new(Vec::new()),
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
    fn ensure_loaded(&self, slot: &str, path: &str, config: Value) -> Result<()> {
        if !self.shared.host.is_loaded(slot) {
            self.shared
                .host
                .load(slot, path, config)
                .with_context(|| format!("loading `{path}` into slot `{slot}`"))?;
        }
        Ok(())
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
        if let Err(e) = fiber.join().await {
            return Err(anyhow::anyhow!("slot `{slot}` fiber failed to start: {e}"));
        }
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
        self.ensure_loaded(slot, path, config)?;
        self.mount_fiber(slot).await
    }

    /// Unload a slot and drop its fiber, removing it from `studio.json`.
    pub async fn unmount_slot(&self, slot: &str) -> Result<()> {
        self.unmount_inner(slot).await?;
        let mut cfg = self.shared.config.lock().unwrap();
        cfg.plugins.remove(slot);
        drop(cfg);
        self.save()?;
        self.fire(StudioEvent::Changed);
        Ok(())
    }

    /// Unmount without persisting (used by the watcher / reload path).
    pub async fn unmount_inner(&self, slot: &str) -> Result<()> {
        self.dispose_fiber(slot).await;
        if self.shared.host.is_loaded(slot) {
            self.shared.host.unload(slot)?;
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
        let result = self.shared.host.reload(slot, &path, None);

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
    pub fn set_slot_config(&self, slot: &str, config: Value) -> Result<bool> {
        let consumed = self.shared.host.apply_config(slot, config.clone())?;
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
    pub fn discover(&self) -> Vec<Discovered> {
        let configured: BTreeMap<String, String> = self
            .config()
            .plugins
            .into_iter()
            .map(|(k, v)| (k, v.path))
            .collect();

        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.shared.plugins_dir) else {
            return out;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("wasm") {
                continue;
            }
            let slot = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("plugin")
                .to_string();
            let path_str = path.display().to_string();
            if configured.get(&slot) == Some(&path_str) {
                continue; // already configured identically
            }
            out.push(Discovered {
                slot,
                path: path_str,
            });
        }
        out.sort_by(|a, b| a.slot.cmp(&b.slot));
        out
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
        AgentRow {
            id: agent.id().to_string(),
            status: match agent.status() {
                dsh_rs::types::AgentStatus::Idle => "idle",
                dsh_rs::types::AgentStatus::Running => "running",
            }
            .to_string(),
            messages: session.derive_messages().len(),
            turns: session.events().len(),
            busy: agent.driver_busy(),
        }
    }

    /// Every live agent.
    pub fn list_agents(&self) -> Vec<AgentRow> {
        match self.agents() {
            Ok(reg) => reg.list().iter().map(|a| self.agent_row(a)).collect(),
            Err(_) => Vec::new(),
        }
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
        let reg = self.agents()?;
        let options = dsh_rs::types::AgentOptions {
            provider,
            model,
            max_tokens: None,
        };
        let agent = reg.create(id, options, cwd, None).map_err(anyhow::Error::msg)?;
        Ok(agent.id().to_string())
    }

    fn agent(&self, id: &str) -> Result<Arc<dyn dsh_rs::api::services::AgentView>> {
        self.agents()?
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("no agent `{id}`"))
    }

    /// Send a user message and wait for the turn to finish.
    pub async fn send_message(
        &self,
        agent_id: &str,
        text: String,
        msg_id: String,
    ) -> Result<()> {
        let agent = self.agent(agent_id)?;
        agent.followup(dsh_rs::types::Message::user(
            msg_id,
            vec![text_block(text)],
        ));
        agent.when_idle().await;
        Ok(())
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
        let reg = self.agents()?;
        if let Some(agent) = reg.get(agent_id) {
            reg.dispose(&agent);
        }
        Ok(())
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
        }
    }
}

/// One `.wasm` found on disk that is not yet configured.
#[derive(Debug, Clone, Serialize)]
pub struct Discovered {
    pub slot: String,
    pub path: String,
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

/// Install the dsh base bundle on `ctx`.
///
/// **Async on purpose.** cordis drives each plugin fiber with a tokio task, so
/// the runtime this runs on must outlive the studio. Tauri owns a global runtime
/// for exactly this reason; `Studio::boot` blocks on it, while tests call
/// [`Studio::with_hook`] inside their own `#[tokio::test]`.
async fn boot_harness(ctx: &cordis::Context) -> Result<()> {
    dsh_rs::bundle::install_base_default(ctx)
        .await
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("dsh base bundle: {e}"))
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
}


// ---------------------------------------------------------------------------
// Chat surface types
// ---------------------------------------------------------------------------

/// One agent as the UI sees it.
#[derive(Debug, Clone, Serialize)]
pub struct AgentRow {
    pub id: String,
    pub status: String,
    pub messages: usize,
    /// Number of session events (turns/steps/tool calls) — a rough activity meter.
    pub turns: usize,
    pub busy: bool,
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
fn register_llm_providers(ctx: &cordis::Context, config: &Config) {
    // The host `Config` has a generic `extra` section precisely so an embedder
    // can keep its own settings in the same file. Ours live under `extra.llm`.
    let Some(llm) = config.extra.as_ref().and_then(|e| e.get("llm")) else {
        return;
    };
    let Some(providers) = llm.get("providers").and_then(|p| p.as_object()) else {
        return;
    };
    let Ok(runtime) = ctx.require::<dsh_rs::api::services::LlmService>(dsh_rs::api::LLM_SERVICE)
    else {
        return;
    };
    for (name, section) in providers {
        // Each section is an OpenAI-compatible endpoint configuration.
        match dsh_rs::llm::adapters::openai::OpenAiAdapter::new(section.clone()) {
            Ok(adapter) => {
                let adapter: Arc<dyn dsh_rs::api::services::LlmAdapterApi> = Arc::new(adapter);
                if let Err(e) = runtime.register_adapter(&[name.as_str()], adapter) {
                    eprintln!("[studio] registering llm provider `{name}` failed: {e}");
                }
            }
            Err(e) => eprintln!("[studio] building llm provider `{name}` failed: {e}"),
        }
    }
}
