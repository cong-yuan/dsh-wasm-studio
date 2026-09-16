//! dsh-wasm-studio — a Tauri backend that composes a dsh-rs agent harness with
//! the `wasm-plugin-host` WASM plugin runtime.
//!
//! The design in one paragraph: a WASM `.wasm` file is a **plugin**. It can
//! expose tools (which the dsh agent loop can call), hook dsh's flow
//! (`tools/pre-execute` and friends, with observe / rewrite / **veto**), and
//! declare `injects`/`provides` so it participates in the cordis service graph
//! exactly like a native dsh plugin. Each WASM slot is mounted as its own
//! cordis plugin, so it has an independent fiber and lifecycle.
//!
//! The Svelte frontend is a Supabase-style admin panel over the command surface
//! in [`commands`]: list/mutate plugins, tools and services, and stream logs.

pub mod commands;
pub mod studio;

use studio::Studio;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // Boot the harness + WASM host once, and keep it as managed state.
            let handle = app.handle().clone();
            let studio = Studio::boot(&handle)
                .map_err(|e| format!("studio failed to boot: {e}"))?;
            app.manage(studio);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::studio_status,
            commands::list_plugins,
            commands::load_plugin,
            commands::unload_plugin,
            commands::set_plugin_enabled,
            commands::reload_plugin,
            commands::set_plugin_config,
            commands::validate_plugin,
            commands::discover_plugins,
            commands::watch_status,
            commands::start_watch,
            commands::stop_watch,
            commands::ui_contributions,
            commands::list_tools,
            commands::call_tool,
            commands::list_services,
            commands::get_logs,
            commands::set_log_level,
            commands::create_agent,
            commands::list_agents,
            commands::send_message,
            commands::steer_agent,
            commands::cancel_agent,
            commands::dispose_agent,
            commands::transcript,
            commands::capabilities,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
