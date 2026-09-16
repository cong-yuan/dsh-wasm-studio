//! The Studio: the long-lived state every Tauri command operates on.
//!
//! It owns the two halves of the system and keeps them in step:
//!
//! * a **dsh-rs harness** (`cordis::Context` with the base bundle installed) —
//!   the agent loop, sessions, tool registry, LLM seam;
//! * a **WASM plugin host** ([`WasmHost`]) mounted into that harness, so each
//!   WASM slot is its own cordis plugin (own fiber, `inject`, `provide`).
//!
//! ## Locking discipline
//!
//! `WasmHost` is `Arc<Mutex<Registry>>` internally, and `Registry` is `Send` but
//! not `Sync`, so all guest calls are serialised behind that mutex. The rule
//! here is: **never hold the registry mutex across an `.await`**. Calls that
//! cross into a guest go through `WasmHost`, which keeps the lock inside a
//! closure; everything the UI does is therefore awaited *outside* any lock.
//!
//! Tauri's managed state must be `Send + Sync`; every type stored here is
//! (verified in `dsh-wasm-host`'s test suite), so no wrapper gymnastics are
//! needed.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use dsh_wasm_host::{install, LoadSpec, Mounted, WasmHost};
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter};

/// State the Tauri commands operate on.
pub struct Studio {
    ctx: cordis::Context,
    host: WasmHost,
    /// Slots currently mounted as cordis plugins. Re-mounting replaces the entry.
    mounted: Mutex<Vec<(String, Mounted)>>,
    /// Directory scanned by default when loading plugins.
    plugins_dir: PathBuf,
    /// Whether the dsh base bundle finished booting.
    booted: bool,
}

impl Studio {
    /// Build a studio with an explicit log hook and plugins directory.
    ///
    /// This is the real constructor; [`Studio::boot`] is the Tauri-flavoured
    /// wrapper that supplies an event-emitting hook. Keeping the Tauri types out
    /// of here lets the boot path be exercised headlessly in tests.
    pub async fn with_hook(
        log_hook: Option<wasm_plugin_host::LogHook>,
        plugins_dir: PathBuf,
    ) -> Result<Self> {
        let ctx = cordis::Context::new();

        let host = WasmHost::with_options(dsh_wasm_host::HostOptions {
            log_capacity: Some(4000),
            echo_stderr: false,
            log_hook,
        })
        .context("building the WASM host")?;

        // Boot the dsh harness (agent loop, sessions, tools, llm seam).
        boot_harness(&ctx).await?;

        // Tell the WASM registry which dsh services WASM plugins may inject.
        for svc in dsh_services() {
            host.declare_dsh_service(svc);
        }

        let _ = std::fs::create_dir_all(&plugins_dir);

        Ok(Self {
            ctx,
            host,
            mounted: Mutex::new(Vec::new()),
            plugins_dir,
            booted: true,
        })
    }

    /// Build the studio in a Tauri app: the log hook forwards every guest line
    /// to the frontend as a `studio://log` event.
    pub fn boot(app: &AppHandle) -> Result<Self> {
        let app_for_logs = app.clone();
        let hook: wasm_plugin_host::LogHook = Arc::new(move |rec: &wasm_plugin_host::LogRecord| {
            // `emit` is cheap and non-blocking; failure just means no window yet.
            let _ = app_for_logs.emit("studio://log", rec);
        });
        // Blocks on Tauri's *global* async runtime, which stays alive for the
        // whole app — so the fiber tasks booted here keep running.
        tauri::async_runtime::block_on(Self::with_hook(Some(hook), plugins_dir(app)))
    }

    /// The cordis context the harness and all mounted slots live on.
    pub fn ctx(&self) -> &cordis::Context {
        &self.ctx
    }

    /// The directory plugins are loaded from by default.
    pub fn plugins_dir(&self) -> &PathBuf {
        &self.plugins_dir
    }

    pub fn host(&self) -> &WasmHost {
        &self.host
    }

    /// Load and mount one plugin.
    ///
    /// If `slot` is already mounted it is unmounted first, so this doubles as a
    /// "replace" operation. `async` because mounting drives cordis fibers; the
    /// guest-side work inside `install` is synchronous but brief.
    pub async fn mount_slot(&self, slot: &str, path: &str, config: Value) -> Result<()> {
        if self.host.is_loaded(slot) {
            self.unmount_slot(slot).await?;
        }
        let spec = LoadSpec::new(slot, path).with_config(config);
        let mounted = install(&self.ctx, self.host.clone(), vec![spec]).await?;
        let mut guard = self.mounted.lock().unwrap();
        guard.push((slot.to_string(), mounted));
        Ok(())
    }

    /// Unmount one slot: drop its fiber (unloading the wasm instance) and forget it.
    pub async fn unmount_slot(&self, slot: &str) -> Result<()> {
        let taken = {
            let mut guard = self.mounted.lock().unwrap();
            guard
                .iter()
                .position(|(s, _)| s == slot)
                .map(|i| guard.remove(i))
        };
        if let Some((_, mounted)) = taken {
            mounted.dispose().await;
        }
        // Even if no fiber was tracked, make sure the registry has no instance.
        if self.host.is_loaded(slot) {
            self.host.unload(slot)?;
        }
        Ok(())
    }

    /// Reload a slot's code, keeping its identity; re-mount so the cordis side
    /// sees the new tool/service surface.
    pub fn reload_slot(&self, slot: &str, path: &str) -> Result<()> {
        self.host.reload(slot, path, None)?;
        Ok(())
    }

    /// Is `slot` currently mounted?
    pub fn is_mounted(&self, slot: &str) -> bool {
        self.mounted
            .lock()
            .unwrap()
            .iter()
            .any(|(s, _)| s == slot)
    }
}

/// Install the dsh base bundle on `ctx`.
///
/// **Async on purpose.** cordis drives each plugin fiber with a tokio task, so
/// the runtime this runs on must outlive the studio — the fibers would stop
/// being polled if it were a temporary runtime that got dropped after boot.
/// Tauri owns a global runtime for exactly this reason; `Studio::boot` blocks
/// on it, while tests call [`Studio::with_hook`] inside their own `#[tokio::test]`.
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

/// Where plugin `.wasm` files live: `<app-data>/plugins`, created on demand.
fn plugins_dir(app: &AppHandle) -> PathBuf {
    use tauri::Manager;
    let base = app
        .path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("dsh-wasm-studio"));
    let dir = base.join("plugins");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// A one-line summary of studio state, for the dashboard header.
#[derive(Debug, Serialize)]
pub struct StudioStatus {
    pub booted: bool,
    pub plugins_dir: String,
    pub slot_count: usize,
    /// Tools contributed by WASM plugins.
    pub wasm_tool_count: usize,
    /// Total tools on the harness registry (WASM + dsh built-ins).
    pub tool_count: usize,
    pub service_count: usize,
}

impl Studio {
    pub fn status(&self) -> StudioStatus {
        let plugins = self.host.list_plugins();
        // The WASM registry only knows guest tools; the dsh registry also holds
        // built-ins (bash, read_file, …). Report both so the dashboard does not
        // imply the harness has no tools when only guests are absent.
        let wasm_tool_count = self.host.list_tools().len();
        let tool_count = self
            .ctx
            .require::<dsh_rs::api::services::ToolsService>(dsh_rs::api::TOOLS_SERVICE)
            .map(|t| t.list().len())
            .unwrap_or(wasm_tool_count);
        StudioStatus {
            booted: self.booted,
            plugins_dir: self.plugins_dir.display().to_string(),
            slot_count: plugins.len(),
            wasm_tool_count,
            tool_count,
            service_count: self.host.services().len(),
        }
    }
}
