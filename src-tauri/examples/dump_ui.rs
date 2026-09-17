//! Print the exact `ui_contributions` payload the frontend receives, after
//! loading the two demo plugins. Pipe this into a Node check to verify the
//! whole chain (real plugin -> command output -> frontend slot registry).
//!
//!   cargo run -p dsh-wasm-studio --example dump_ui | node src/lib/e2e-check.mjs

use dsh_wasm_studio_lib::studio::Studio;

fn main() -> anyhow::Result<()> {
    let handle = tauri::async_runtime::handle().inner().clone();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("wasm-plugin-host/target/wasm32-wasip1/release");
    let a = root.join("ui_llm_panel.wasm");
    let b = root.join("ui_theme_widget.wasm");
    if !a.exists() || !b.exists() {
        eprintln!("build the demo plugins first");
        std::process::exit(2);
    }

    let studio = handle.block_on(async {
        // A clean app-data dir, so the check is deterministic. Discovery is
        // still exercised (the plugin dir + configured/build-output roots).
        let dir = std::env::temp_dir().join("studio-dump-ui");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        Studio::with_hook(None, None, dir).await
    })?;

    // Also show what discovery reports, since that is what the UI calls.
    let disc = studio.discover();
    eprintln!("discovery: {} plugin(s) found", disc.plugins.len());
    for p in &disc.plugins {
        eprintln!("  found {} -> {}", p.slot, p.path);
    }
    for r in &disc.searched {
        eprintln!(
            "  searched [{}] {} (exists={}, new wasm={})",
            r.label, r.path, r.exists, r.wasm_count
        );
    }

    // Show the catalog after starting one plugin: it must contain BOTH the
    // running one and the not-yet-started one.
    handle.block_on(studio.mount_slot("ui-llm-panel", &a.display().to_string(), serde_json::Value::Null))?;
    eprintln!("--- catalog after starting only ui-llm-panel ---");
    for e in studio.catalog() {
        eprintln!(
            "  {:<16} running={:<5} state={:<10} in_config={:<5} exists={}",
            e.slot, e.running, e.state, e.in_config, e.exists
        );
    }
    // And confirm stopping keeps it listed.
    handle.block_on(studio.unmount_slot("ui-llm-panel"))?;
    eprintln!("--- catalog after STOPPING ui-llm-panel (must still list it) ---");
    for e in studio.catalog() {
        eprintln!(
            "  {:<16} running={:<5} state={:<10} in_config={}",
            e.slot, e.running, e.state, e.in_config
        );
    }
    handle.block_on(studio.mount_slot("ui-llm-panel", &a.display().to_string(), serde_json::Value::Null))?;
    handle.block_on(studio.mount_slot("ui-theme-widget", &b.display().to_string(), serde_json::Value::Null))?;

    eprintln!("--- declared plugin windows ---");
    for w in studio.plugin_windows() {
        eprintln!(
            "  label={:<32} slot={:<14} component={:<12} open={}",
            w.label, w.slot, w.component, w.open
        );
    }

    // Mirror `commands::ui_contributions` exactly.
    let out: Vec<serde_json::Value> = studio
        .host()
        .ui_decls()
        .into_iter()
        .map(|(slot, ui)| {
            serde_json::json!({
                "slot": slot,
                "provides_slots": ui.provides.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
                "injects_slots": ui.injects.iter().map(|i| serde_json::json!({
                    "slot": i.slot,
                    "priority": i.priority,
                    "component": i.component,
                })).collect::<Vec<_>>(),
                "assets": ui.assets,
            })
        })
        .collect();
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
