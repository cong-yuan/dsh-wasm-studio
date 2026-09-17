//! Boot the studio's backend exactly as the app does, then drive a full agent
//! turn — without Tauri, so it can be run from a plain `cargo run`.
//!
//! This is the manual counterpart to `tests/studio.rs`: it exercises the
//! *off-runtime* path that a synchronous Tauri command would take, which is
//! where the "no reactor running" crash lived.

use dsh_wasm_studio_lib::studio::Studio;
use serde_json;

fn main() -> anyhow::Result<()> {
    // Tauri normally owns a global runtime; mimic that here.
    // An **explicit** runtime — see the note in `dump_ui.rs` for why borrowing
    // Tauri's lazily-created global handle and `block_on`-ing it deadlocks
    // intermittently outside a Tauri app.
    // An **explicit** runtime, not `tauri::async_runtime::handle()`.
    //
    // Outside a Tauri app that accessor lazily creates a global runtime and
    // hands back a handle to it, which we then `block_on` from the main
    // thread — a self-reference that deadlocks intermittently. Owning the
    // runtime here removes it. (A *current-thread* runtime also avoids
    // cordis's `Fiber::join` lost-wakeup race, but it cannot drive the
    // concurrent work these examples touch, so multi-thread it is.)
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let handle = rt.handle().clone();
    let studio = handle.block_on(async {
        let dir = std::env::temp_dir().join("studio-headless");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        Studio::with_hook(None, None, dir).await
    })?;

    println!("booted: {}", studio.status().booted);

    // Create an agent from a thread with NO runtime context — the exact
    // situation a synchronous Tauri command is in.
    let s2 = studio.clone();
    std::thread::spawn(move || {
        let id = s2.create_agent(
            Some("demo".into()),
            "mock".into(),
            "mock-1".into(),
            Some("/tmp".into()),
        )?;
        println!("created agent: {id}");
        anyhow::Ok(())
    })
    .join()
    .expect("thread must not panic")?;

    // Load the two demo UI plugins if they have been built, and print what the
    // frontend would receive. This is the manual check for the UI chain.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
    let rel = root.join("wasm-plugin-host/target/wasm32-wasip1/release");
    let a = rel.join("ui_llm_panel.wasm");
    let b = rel.join("ui_theme_widget.wasm");
    if a.exists() && b.exists() {
        handle.block_on(studio.mount_slot("ui-llm-panel", &a.display().to_string(), serde_json::Value::Null))?;
        handle.block_on(studio.mount_slot("ui-theme-widget", &b.display().to_string(), serde_json::Value::Null))?;
        println!("--- ui declarations the frontend receives ---");
        for (slot, ui) in studio.host().ui_decls() {
            println!("plugin `{slot}`:");
            for p in &ui.provides {
                println!("  opens slot: {}", p.name);
            }
            for i in &ui.injects {
                println!("  mounts into: {} (component={:?}, priority={})", i.slot, i.component, i.priority);
            }
            println!("  assets: {:?}", ui.assets.keys().collect::<Vec<_>>());
        }
    } else {
        println!("(demo UI plugins not built; skip)");
    }

    // Send a message (mock echoes) and print the transcript.
    handle.block_on(studio.send_message("demo", "hello".into(), "u1".into()))?;
    for m in studio.transcript("demo")? {
        println!("[{}] {}", m.role, if m.text.is_empty() { "(no text)" } else { &m.text });
    }

    println!("OK — no panic");
    Ok(())
}
