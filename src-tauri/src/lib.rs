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

/// Is *any* window currently on screen?
///
/// The app's own window starts hidden so a plugin owning the launch view does
/// not let the default page flash first. `Studio::boot` guarantees it shows
/// exactly one window, so this is a backstop for the catastrophic case only:
/// if boot somehow left nothing visible, a user would have no way to interact
/// with the app at all. A window we cannot query counts as not visible.
fn any_window_is_visible(app: &tauri::AppHandle) -> bool {
    use tauri::Manager;
    app.webview_windows()
        .values()
        .any(|w| w.is_visible().unwrap_or(false))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            use tauri::Manager;

            // Boot the harness + WASM host once, and keep it as managed state.
            let handle = app.handle().clone();
            let studio = match Studio::boot(&handle) {
                Ok(s) => s,
                Err(e) => {
                    // The app window starts hidden (a plugin may own the launch
                    // view), and setup errors panic — so show the window and a
                    // readable reason before that happens, rather than exiting
                    // with no window ever having appeared.
                    if let Some(main) = handle.get_webview_window("main") {
                        let _ = main.set_title(&format!("WASM Studio — boot failed: {e}"));
                        let _ = main.show();
                    }
                    return Err(format!("studio failed to boot: {e}").into());
                }
            };

            // Last line of defence for the hidden `main` window: if boot left
            // nothing visible (a plugin window failed to open, say), show the
            // app window. A user must never be left with no window at all.
            if !any_window_is_visible(&handle) {
                if let Some(main) = handle.get_webview_window("main") {
                    let _ = main.show();
                }
            }

            app.manage(studio);
            Ok(())
        })
        .on_window_event(|window, event| {
            // If the window owning the launch view is closed, the hidden app
            // window becomes the only thing left — and it is hidden. Reveal it,
            // so closing a plugin window returns the user to the app instead of
            // leaving a process with nothing on screen.
            //
            // The closed window must not *be* main: the user closing the app
            // window means close it, not re-open it. Without this guard the
            // handler sees "nothing visible" (main is on its way out) and calls
            // `show()` on the very window being destroyed — harmless today, but
            // wrong, and it logged a fallback that never happened.
            if let tauri::WindowEvent::Destroyed = event {
                let app = window.app_handle();
                let label = window.label().to_string();
                if label != "main" && !any_window_is_visible(app) {
                    if let Some(main) = app.get_webview_window("main") {
                        let _ = main.show();
                        let _ = main.set_focus();
                    }
                    eprintln!("[studio] window `{label}` was the last one; showing the app window");
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::studio_status,
            commands::plugin_windows,
            commands::plugin_window_for,
            commands::open_plugin_window,
            commands::open_plugin_window_with,
            commands::send_window_params,
            commands::close_plugin_window,
            commands::plugin_catalog,
            commands::list_plugins,
            commands::load_plugin,
            commands::unload_plugin,
            commands::remove_plugin,
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
            commands::get_llm_config,
            commands::set_llm_config,
            commands::fetch_llm_models,
            commands::sync_llm_adapters,
            commands::create_agent,
            commands::list_agents,
            commands::list_sessions,
            commands::resume_session,
            commands::send_message,
            commands::steer_agent,
            commands::cancel_agent,
            commands::dispose_agent,
            commands::soft_unbind_agent,
            commands::rebind_agent_model,
            commands::list_models,
            commands::transcript,
            commands::chat_partial,
            commands::capabilities,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
