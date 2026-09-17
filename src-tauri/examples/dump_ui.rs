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
    let c = root.join("ui_curator.wasm");
    let d = root.join("ui_multifile.wasm");
    if !a.exists() || !b.exists() || !c.exists() || !d.exists() {
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
    // Loaded LAST: it only adjusts the two above, proving a later plugin can
    // reshape UI that already exists.
    handle.block_on(studio.mount_slot("ui-curator", &c.display().to_string(), serde_json::Value::Null))?;
    // A multi-file plugin: its UI is split across four `.js` assets.
    handle.block_on(studio.mount_slot("ui-multifile", &d.display().to_string(), serde_json::Value::Null))?;

    eprintln!("--- declared plugin windows ---");
    for w in studio.plugin_windows() {
        eprintln!(
            "  label={:<34} content={:<5} component={:<12} open={}",
            w.label, w.content, w.component, w.open
        );
        if w.content == "html" {
            match studio.window_html(&w.label) {
                Some(h) => eprintln!("      html: {} bytes", h.len()),
                None => eprintln!("      html: MISSING"),
            }
        }
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
                "routes": ui.routes.iter().map(|r| serde_json::json!({
                    "path": r.path,
                    "component": r.component,
                    "title": r.title,
                    "icon": r.icon,
                    "nav": r.nav,
                })).collect::<Vec<_>>(),
                "adjusts": ui.adjusts.iter().map(|a| {
                    let mut m = serde_json::Map::new();
                    m.insert("slot".into(), serde_json::json!(a.slot));
                    if let Some(f) = &a.from { m.insert("from".into(), serde_json::json!(f)); }
                    m.insert("action".into(), serde_json::json!(match a.action {
                        wasm_plugin_host::AdjustAction::Hide => "hide",
                        wasm_plugin_host::AdjustAction::Unhide => "unhide",
                        wasm_plugin_host::AdjustAction::Replace => "replace",
                        wasm_plugin_host::AdjustAction::Priority => "priority",
                    }));
                    if let Some(t) = a.to { m.insert("to".into(), serde_json::json!(t)); }
                    if let Some(b) = a.by { m.insert("by".into(), serde_json::json!(b)); }
                    if let Some(k) = &a.component { m.insert("component".into(), serde_json::json!(k)); }
                    serde_json::Value::Object(m)
                }).collect::<Vec<_>>(),
            })
        })
        .collect();
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
