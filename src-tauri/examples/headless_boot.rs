//! Boot the studio's backend exactly as the app does, then drive a full agent
//! turn — without Tauri, so it can be run from a plain `cargo run`.
//!
//! This is the manual counterpart to `tests/studio.rs`: it exercises the
//! *off-runtime* path that a synchronous Tauri command would take, which is
//! where the "no reactor running" crash lived.

use dsh_wasm_studio_lib::studio::Studio;

fn main() -> anyhow::Result<()> {
    // Tauri normally owns a global runtime; mimic that here.
    let handle = tauri::async_runtime::handle().inner().clone();
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

    // Send a message (mock echoes) and print the transcript.
    handle.block_on(studio.send_message("demo", "hello".into(), "u1".into()))?;
    for m in studio.transcript("demo")? {
        println!("[{}] {}", m.role, if m.text.is_empty() { "(no text)" } else { &m.text });
    }

    println!("OK — no panic");
    Ok(())
}
