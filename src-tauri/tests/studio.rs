//! Backend integration tests: boot the studio headlessly and drive it.
//!
//! These run without a Tauri window, so the boot path, plugin mounting and the
//! command logic are all exercised in CI. Plugins are tiny WAT modules compiled
//! in-process, so no `cargo build` of a wasm plugin is needed.

use std::path::PathBuf;

use dsh_wasm_studio_lib::studio::Studio;
use serde_json::json;

const RESULT: &str = r#"{"kind":"success","content":"ran","value":{"ok":true}}"#;

/// A minimal ABI-v1 module declaring one tool `<slot>_tool`.
fn wasm_tool(slot: &str) -> Vec<u8> {
    let decl = format!(
        r#"{{"name":"{slot}","abi":1,"tools":[{{"name":"{slot}_tool","description":"t","exec":"go"}}]}}"#
    );
    let decl_off = 64usize;
    let res_off = decl_off + decl.len();
    let wat = format!(
        r#"(module
          (import "host" "log" (func $log (param i32 i32 i32)))
          (memory (export "memory") 2)
          (global $bump (mut i32) (i32.const 8192))
          (data (i32.const {decl_off}) {decl:?})
          (data (i32.const {res_off}) {res:?})
          (func (export "plugin_abi_version") (result i32) (i32.const 1))
          (func (export "plugin_init") (result i32) (i32.const 0))
          (func (export "plugin_shutdown"))
          (func (export "plugin_alloc") (param $n i32) (result i32)
            (local $p i32)
            (local.set $p (global.get $bump))
            (global.set $bump (i32.add (global.get $bump) (local.get $n)))
            (local.get $p))
          (func (export "plugin_free") (param i32 i32))
          (func $blit (param $src i32) (param $len i32) (param $out i32) (param $cap i32) (result i64)
            (local $i i32)
            (if (i32.lt_s (local.get $cap) (local.get $len))
              (then (return (i64.extend_i32_s (i32.sub (i32.const 0) (local.get $len))))))
            (block $done (loop $loop
              (br_if $done (i32.ge_s (local.get $i) (local.get $len)))
              (i32.store8 (i32.add (local.get $out) (local.get $i))
                (i32.load8_u (i32.add (local.get $src) (local.get $i))))
              (local.set $i (i32.add (local.get $i) (i32.const 1)))
              (br $loop)))
            (i64.extend_i32_s (local.get $len)))
          (func (export "plugin_describe") (param $out i32) (param $cap i32) (result i64)
            (call $blit (i32.const {decl_off}) (i32.const {dlen}) (local.get $out) (local.get $cap)))
          (func (export "plugin_invoke")
            (param i32 i32 i32 i32) (param $out i32) (param $cap i32) (result i64)
            (call $blit (i32.const {res_off}) (i32.const {rlen}) (local.get $out) (local.get $cap)))
        )"#,
        decl_off = decl_off,
        res_off = res_off,
        decl = decl,
        res = RESULT,
        dlen = decl.len(),
        rlen = RESULT.len(),
    );
    wat::parse_str(&wat).expect("test wat should parse")
}

fn tmpdir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("studio-test-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[tokio::test]
async fn studio_boots_with_the_dsh_harness() {
    let dir = tmpdir("boot");
    let studio = Studio::with_hook(None, None, dir).await.expect("studio boots");

    let status = studio.status();
    assert!(status.booted, "harness must report booted");
    // The dsh base bundle registers built-in tools (bash, read_file, …).
    assert!(
        status.tool_count >= 7,
        "dsh built-in tools should be present, got {}",
        status.tool_count
    );
    // …and its services are visible to WASM plugins.
    let services = studio.host().services();
    assert!(
        services.iter().any(|s| s == "tools"),
        "dsh `tools` service should be declared, got {services:?}"
    );
}

#[tokio::test]
async fn a_plugin_can_be_mounted_called_and_unmounted() {
    let dir = tmpdir("mount");
    let wasm = dir.join("alpha.wasm");
    std::fs::write(&wasm, wasm_tool("alpha")).unwrap();

    let studio = Studio::with_hook(None, None, dir.clone()).await.unwrap();
    let path = wasm.display().to_string();

    // Mount.
    studio
        .mount_slot("alpha", &path, json!(null))
        .await
        .expect("mount succeeds");

    let plugins = studio.host().list_plugins();
    assert_eq!(plugins.len(), 1, "one slot mounted, got {plugins:?}");
    assert_eq!(plugins[0].0, "alpha");
    assert!(studio.is_mounted("alpha"));

    // The tool is on the registry and callable.
    let tools = studio.host().list_tools();
    assert!(tools.iter().any(|t| t.name == "alpha_tool"), "got {tools:?}");
    let out = studio.host().call_tool("alpha_tool", &json!({})).unwrap();
    assert_eq!(out["kind"], "success");

    // Unmount really releases the slot (this is the whole point of the host).
    studio.unmount_slot("alpha").await.unwrap();
    assert!(studio.host().list_plugins().is_empty(), "slot must be gone");
    assert!(!studio.is_mounted("alpha"));
    assert!(studio.host().list_tools().is_empty(), "its tool must be gone");
}

#[tokio::test]
async fn mounting_the_same_slot_twice_replaces_it() {
    let dir = tmpdir("replace");
    let wasm = dir.join("alpha.wasm");
    std::fs::write(&wasm, wasm_tool("alpha")).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let path = wasm.display().to_string();
    studio.mount_slot("alpha", &path, json!(null)).await.unwrap();
    studio.mount_slot("alpha", &path, json!(null)).await.unwrap();

    assert_eq!(studio.host().list_plugins().len(), 1, "replace, not duplicate");
}

#[tokio::test]
async fn applying_config_reaches_the_guest() {
    let dir = tmpdir("config");
    let wasm = dir.join("alpha.wasm");
    std::fs::write(&wasm, wasm_tool("alpha")).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let path = wasm.display().to_string();
    studio
        .mount_slot("alpha", &path, json!({ "greeting": "Hi" }))
        .await
        .unwrap();

    // A live config push reports whether the guest consumed it (it has no
    // plugin_on_config hook here, so `false` — but it must not error).
    let consumed = studio
        .host()
        .apply_config("alpha", json!({ "greeting": "Hello" }))
        .unwrap();
    assert!(!consumed, "a plugin without a config hook consumes nothing");
}

#[tokio::test]
async fn loading_a_nonexistent_file_fails_cleanly() {
    let dir = tmpdir("missing");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let err = studio
        .mount_slot("ghost", "/does/not/exist.wasm", json!(null))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("loading") || err.to_string().contains("not exist"),
        "expected a clear load error, got: {err}"
    );
}

#[tokio::test]
async fn a_broken_reload_leaves_the_running_plugin_intact() {
    let dir = tmpdir("atomic");
    let wasm = dir.join("alpha.wasm");
    std::fs::write(&wasm, wasm_tool("alpha")).unwrap();

    let studio = Studio::with_hook(None, None, dir.clone()).await.unwrap();
    let path = wasm.display().to_string();
    studio.mount_slot("alpha", &path, json!(null)).await.unwrap();

    // Corrupt the file, then try to reload: the atomic hot-swap must reject it
    // and keep the working plugin running.
    std::fs::write(&wasm, b"not a wasm module").unwrap();
    let err = studio.reload_slot("alpha").await;
    assert!(err.is_err(), "a broken build must be rejected");

    // Still loaded and callable.
    assert_eq!(studio.host().list_plugins().len(), 1);
    let out = studio.host().call_tool("alpha_tool", &json!({})).unwrap();
    assert_eq!(out["kind"], "success", "the old plugin keeps serving");
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loading_a_plugin_persists_it_to_the_config_file() {
    let dir = tmpdir("persist");
    let wasm = dir.join("alpha.wasm");
    std::fs::write(&wasm, wasm_tool("alpha")).unwrap();

    let studio = Studio::with_hook(None, None, dir.clone()).await.unwrap();
    let path = wasm.display().to_string();
    studio.mount_slot("alpha", &path, json!({ "k": 1 })).await.unwrap();

    // The desired state must be on disk, in the host's own Config format…
    let cfg = wasm_plugin_host::Config::load(studio.config_path()).unwrap();
    let entry = cfg.plugins.get("alpha").expect("alpha persisted");
    assert_eq!(entry.path, path);
    assert!(entry.enabled);
    assert_eq!(entry.config, Some(json!({ "k": 1 })));

    // Stopping keeps the entry but marks it disabled — "stop" is not "forget".
    studio.unmount_slot("alpha").await.unwrap();
    let cfg = wasm_plugin_host::Config::load(studio.config_path()).unwrap();
    let entry = cfg.plugins.get("alpha").expect("stop keeps the entry");
    assert!(!entry.enabled, "a stopped plugin is disabled, not deleted");

    // Removing is the explicit way to forget it.
    studio.remove_slot("alpha").await.unwrap();
    let cfg = wasm_plugin_host::Config::load(studio.config_path()).unwrap();
    assert!(cfg.plugins.get("alpha").is_none(), "remove_slot forgets it");
}

#[tokio::test]
async fn enabled_plugins_autoload_on_next_boot() {
    let dir = tmpdir("autoload");
    let wasm = dir.join("alpha.wasm");
    std::fs::write(&wasm, wasm_tool("alpha")).unwrap();
    let path = wasm.display().to_string();

    // First boot: load and persist.
    {
        let studio = Studio::with_hook(None, None, dir.clone()).await.unwrap();
        studio.mount_slot("alpha", &path, json!(null)).await.unwrap();
        studio.shutdown().await;
    }

    // Second boot over the same app-data dir: the plugin must come back.
    let studio = Studio::with_hook(None, None, dir.clone()).await.unwrap();
    assert_eq!(
        studio.host().list_plugins().len(),
        1,
        "the persisted plugin should have been autoloaded"
    );
    let out = studio.host().call_tool("alpha_tool", &json!({})).unwrap();
    assert_eq!(out["kind"], "success");
}

#[tokio::test]
async fn a_disabled_plugin_is_not_autoloaded() {
    let dir = tmpdir("disabled-autoload");
    let wasm = dir.join("alpha.wasm");
    std::fs::write(&wasm, wasm_tool("alpha")).unwrap();
    let path = wasm.display().to_string();

    {
        let studio = Studio::with_hook(None, None, dir.clone()).await.unwrap();
        studio.mount_slot("alpha", &path, json!(null)).await.unwrap();
        studio.set_enabled("alpha", false).await.unwrap();
        assert_eq!(studio.host().list_plugins().len(), 0, "disabled => unloaded");
    }

    let studio = Studio::with_hook(None, None, dir.clone()).await.unwrap();
    assert!(
        studio.host().list_plugins().is_empty(),
        "a disabled plugin must not autoload"
    );
}

#[tokio::test]
async fn a_broken_persisted_plugin_does_not_stop_boot() {
    let dir = tmpdir("bad-persist");
    let wasm = dir.join("alpha.wasm");
    std::fs::write(&wasm, wasm_tool("alpha")).unwrap();
    let path = wasm.display().to_string();

    {
        let studio = Studio::with_hook(None, None, dir.clone()).await.unwrap();
        studio.mount_slot("alpha", &path, json!(null)).await.unwrap();
        studio.shutdown().await;
    }
    // Corrupt the wasm on disk, then reboot: boot must survive and report it.
    std::fs::write(&wasm, b"garbage").unwrap();
    let studio = Studio::with_hook(None, None, dir.clone()).await.unwrap();
    assert!(studio.status().booted, "boot must not fail on a bad plugin");
}

// ---------------------------------------------------------------------------
// Hot reload changes the tool surface
// ---------------------------------------------------------------------------

/// A module declaring one tool with a caller-chosen name.
fn wasm_tool_named(slot: &str, tool: &str) -> Vec<u8> {
    let decl = format!(
        r#"{{"name":"{slot}","abi":1,"tools":[{{"name":"{tool}","description":"t","exec":"go"}}]}}"#
    );
    let decl_off = 64usize;
    let res_off = decl_off + decl.len();
    let wat = format!(
        r#"(module
          (import "host" "log" (func $log (param i32 i32 i32)))
          (memory (export "memory") 2)
          (global $bump (mut i32) (i32.const 8192))
          (data (i32.const {decl_off}) {decl:?})
          (data (i32.const {res_off}) {res:?})
          (func (export "plugin_abi_version") (result i32) (i32.const 1))
          (func (export "plugin_init") (result i32) (i32.const 0))
          (func (export "plugin_shutdown"))
          (func (export "plugin_alloc") (param $n i32) (result i32)
            (local $p i32)
            (local.set $p (global.get $bump))
            (global.set $bump (i32.add (global.get $bump) (local.get $n)))
            (local.get $p))
          (func (export "plugin_free") (param i32 i32))
          (func $blit (param $src i32) (param $len i32) (param $out i32) (param $cap i32) (result i64)
            (local $i i32)
            (if (i32.lt_s (local.get $cap) (local.get $len))
              (then (return (i64.extend_i32_s (i32.sub (i32.const 0) (local.get $len))))))
            (block $done (loop $loop
              (br_if $done (i32.ge_s (local.get $i) (local.get $len)))
              (i32.store8 (i32.add (local.get $out) (local.get $i))
                (i32.load8_u (i32.add (local.get $src) (local.get $i))))
              (local.set $i (i32.add (local.get $i) (i32.const 1)))
              (br $loop)))
            (i64.extend_i32_s (local.get $len)))
          (func (export "plugin_describe") (param $out i32) (param $cap i32) (result i64)
            (call $blit (i32.const {decl_off}) (i32.const {dlen}) (local.get $out) (local.get $cap)))
          (func (export "plugin_invoke")
            (param i32 i32 i32 i32) (param $out i32) (param $cap i32) (result i64)
            (call $blit (i32.const {res_off}) (i32.const {rlen}) (local.get $out) (local.get $cap)))
        )"#,
        decl_off = decl_off,
        res_off = res_off,
        decl = decl,
        res = RESULT,
        dlen = decl.len(),
        rlen = RESULT.len(),
    );
    wat::parse_str(&wat).expect("test wat should parse")
}

#[tokio::test]
async fn hot_reload_swaps_the_tool_surface() {
    let dir = tmpdir("reload-surface");
    let wasm = dir.join("alpha.wasm");
    std::fs::write(&wasm, wasm_tool_named("alpha", "old_tool")).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let path = wasm.display().to_string();
    studio.mount_slot("alpha", &path, json!(null)).await.unwrap();
    assert!(studio.host().list_tools().iter().any(|t| t.name == "old_tool"));

    // Rebuild with a different tool name, same path (a rebuild in place).
    std::fs::write(&wasm, wasm_tool_named("alpha", "new_tool")).unwrap();

    let tools = studio.reload_slot("alpha").await.expect("reload succeeds");
    assert_eq!(tools, vec!["new_tool".to_string()], "reports the new surface");
    assert!(
        studio.host().list_tools().iter().any(|t| t.name == "new_tool"),
        "new tool must be registered"
    );
    assert!(
        !studio.host().list_tools().iter().any(|t| t.name == "old_tool"),
        "old tool must be gone"
    );
}

#[tokio::test]
async fn reload_surfaces_the_new_tool_on_the_dsh_registry() {
    // The point of remounting the fiber: dsh's own tool registry (what the
    // agent loop sees) must reflect the new surface too, not just the wasm one.
    let dir = tmpdir("reload-dsh");
    let wasm = dir.join("alpha.wasm");
    std::fs::write(&wasm, wasm_tool_named("alpha", "before_tool")).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let path = wasm.display().to_string();
    studio.mount_slot("alpha", &path, json!(null)).await.unwrap();

    let dsh_tools = studio
        .ctx()
        .require::<dsh_rs::api::services::ToolsService>(dsh_rs::api::TOOLS_SERVICE)
        .unwrap();
    assert!(dsh_tools.list().contains(&"before_tool".to_string()));

    std::fs::write(&wasm, wasm_tool_named("alpha", "after_tool")).unwrap();
    studio.reload_slot("alpha").await.unwrap();

    let names = dsh_tools.list();
    assert!(names.contains(&"after_tool".to_string()), "got {names:?}");
    assert!(!names.contains(&"before_tool".to_string()), "got {names:?}");
}

#[tokio::test]
async fn discover_finds_wasm_files_in_the_plugins_dir() {
    let dir = tmpdir("discover");
    let plugins = dir.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(plugins.join("found.wasm"), wasm_tool("found")).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let found = studio.discover();
    assert_eq!(found.plugins.len(), 1, "got {found:?}");
    assert_eq!(found.plugins[0].slot, "found");
}

// ---------------------------------------------------------------------------
// Auto-reload watcher (end to end)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_watcher_hot_reloads_a_rebuilt_plugin() {
    // The real feature: touch the .wasm on disk and the running plugin must be
    // hot-swapped, with no UI action. Uses a recording change hook to observe it.
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let dir = tmpdir("watcher");
    let wasm = dir.join("alpha.wasm");
    std::fs::write(&wasm, wasm_tool_named("alpha", "v1_tool")).unwrap();

    let seen = Arc::new(AtomicUsize::new(0));
    let seen2 = seen.clone();
    let hook: dsh_wasm_studio_lib::studio::ChangeHook =
        Arc::new(move |_ev| {
            seen2.fetch_add(1, Ordering::SeqCst);
        });

    let studio = Studio::with_hook(None, Some(hook), dir.clone())
        .await
        .unwrap();
    let path = wasm.display().to_string();
    studio.mount_slot("alpha", &path, json!(null)).await.unwrap();
    assert!(studio
        .host()
        .list_tools()
        .iter()
        .any(|t| t.name == "v1_tool"));

    // Start watching, then rebuild in place with a new tool name.
    studio.start_watch();
    assert!(studio.watching(), "watcher should have started");

    // Rebuild in place, and **keep rewriting until it is observed**.
    //
    // The first write can land before the watcher has seeded its mtime
    // baseline, in which case it is indistinguishable from the seeded state and
    // that one change is not reported. That window is inherent to any
    // mtime-polling watcher and is not what this test is about: the guarantee
    // under test is "a rebuilt plugin is hot-reloaded", not "a write that races
    // watcher installation is caught". Retrying removes the race without
    // weakening the assertion — if hot reload were broken, no number of writes
    // would pass.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut reloaded = false;
    while std::time::Instant::now() < deadline {
        std::fs::write(&wasm, wasm_tool_named("alpha", "v2_tool")).unwrap();
        // Give the watcher a moment to observe this write.
        for _ in 0..10 {
            if studio
                .host()
                .list_tools()
                .iter()
                .any(|t| t.name == "v2_tool")
            {
                reloaded = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if reloaded {
            break;
        }
    }

    studio.stop_watch();
    assert!(reloaded, "the watcher must hot-reload the rebuilt .wasm");
    assert!(seen.load(Ordering::SeqCst) >= 1, "a change hook should have fired");
}

#[tokio::test]
async fn shutdown_disposes_every_fiber_without_leaking() {
    let dir = tmpdir("shutdown");
    let wasm = dir.join("alpha.wasm");
    std::fs::write(&wasm, wasm_tool("alpha")).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let path = wasm.display().to_string();
    studio.mount_slot("alpha", &path, json!(null)).await.unwrap();
    studio.start_watch();

    studio.shutdown().await;

    assert!(!studio.watching(), "shutdown stops the watcher");
    assert!(!studio.is_mounted("alpha"), "shutdown disposes slot fibers");
}

#[tokio::test]
async fn the_config_cache_setting_enables_the_disk_cache() {
    // The first boot writes a default studio.json with the cache on; the runtime
    // built from it must actually have the disk cache enabled.
    let dir = tmpdir("cache-cfg");
    let studio = Studio::with_hook(None, None, dir.clone()).await.unwrap();

    let enabled = studio
        .host()
        .registry()
        .lock()
        .unwrap()
        .runtime()
        .disk_cache_enabled();
    assert!(enabled, "the default config enables the disk compile cache");
    assert!(
        studio.config_path().exists(),
        "a default studio.json should have been written"
    );
}

#[tokio::test]
async fn disabling_the_cache_in_config_turns_it_off() {
    let dir = tmpdir("cache-off");
    // Pre-write a config with the cache explicitly disabled.
    std::fs::write(
        dir.join("studio.json"),
        r#"{ "cache": { "dir": "cwasm", "enabled": false }, "plugins": {} }"#,
    )
    .unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let enabled = studio
        .host()
        .registry()
        .lock()
        .unwrap()
        .runtime()
        .disk_cache_enabled();
    assert!(!enabled, "enabled=false in config must disable the cache");
}

// ---------------------------------------------------------------------------
// Agents / chat
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_agent_runs_a_turn_over_the_mock_provider() {
    let dir = tmpdir("agent-turn");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();

    let id = studio
        .create_agent(Some("a1".into()), "mock".into(), "mock-1".into(), Some("/tmp".into()))
        .expect("agent created");
    assert_eq!(id, "a1");

    // The mock adapter echoes the user text when unscripted.
    studio
        .send_message("a1", "hello there".into(), "u1".into())
        .await
        .expect("turn completes");

    let transcript = studio.transcript("a1").unwrap();
    let user = transcript.iter().find(|m| m.role == "user").expect("user msg");
    assert_eq!(user.text, "hello there");
    let assistant = transcript
        .iter()
        .find(|m| m.role == "assistant")
        .expect("assistant reply");
    assert_eq!(assistant.text, "hello there", "mock echoes the input");
}

#[tokio::test]
async fn an_agent_calls_a_wasm_tool_in_a_real_turn() {
    // The headline: the agent loop dispatches into a WASM plugin's tool and the
    // result lands in the session transcript. Requires scripting the mock
    // provider to request the tool, exactly as dsh's own test does.
    use dsh_rs::api::services::LlmService;
    use std::sync::Arc;

    let dir = tmpdir("agent-tool");
    let wasm = dir.join("alpha.wasm");
    std::fs::write(&wasm, wasm_tool("alpha")).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .mount_slot("alpha", &wasm.display().to_string(), json!(null))
        .await
        .unwrap();

    // Script: first request the tool, then finish with text.
    let runtime = studio
        .ctx()
        .require::<LlmService>(dsh_rs::api::LLM_SERVICE)
        .unwrap();
    runtime.unregister_adapter(&["mock"]);
    runtime
        .register_adapter(
            &["mock"],
            Arc::new(dsh_rs::llm::adapters::mock::MockAdapter::scripted(vec![
                dsh_rs::llm::adapters::mock::MockAdapter::tool_call_response(
                    "call-1",
                    "alpha_tool",
                    json!({}),
                ),
                dsh_rs::llm::adapters::mock::MockAdapter::text_response("done"),
            ])),
        )
        .unwrap();

    studio
        .create_agent(Some("a1".into()), "mock".into(), "mock-1".into(), Some("/tmp".into()))
        .unwrap();
    studio
        .send_message("a1", "call the tool".into(), "u1".into())
        .await
        .unwrap();

    let transcript = studio.transcript("a1").unwrap();
    // The tool call was requested…
    let calls: Vec<_> = transcript
        .iter()
        .flat_map(|m| m.tool_calls.iter())
        .collect();
    assert!(
        calls.iter().any(|c| c.name == "alpha_tool"),
        "the model's tool call should be in the transcript, got {transcript:?}"
    );
    let call = calls.iter().find(|c| c.id == "call-1").expect("call is projected");
    assert!(call.started_at.is_some(), "tool/call event time must reach transcript");
    // …and its result came back from the WASM guest.
    let results: Vec<_> = transcript
        .iter()
        .flat_map(|m| m.tool_results.iter())
        .collect();
    assert!(!results.is_empty(), "a tool result should be present");
    assert!(
        results.iter().any(|r| r.content.contains("ran")),
        "the wasm guest's content should reach the transcript, got {results:?}"
    );
    let result = results
        .iter()
        .find(|r| r.tool_call_id == "call-1")
        .expect("result is projected");
    assert!(
        result.finished_at >= call.started_at,
        "tool/result event time must follow its call: {call:?} {result:?}"
    );
    // The turn closed with the model's final text.
    assert_eq!(transcript.last().unwrap().text, "done");
}

#[tokio::test]
async fn two_agents_have_independent_transcripts() {
    let dir = tmpdir("two-agents");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio.create_agent(Some("a".into()), "mock".into(), "m".into(), None).unwrap();
    studio.create_agent(Some("b".into()), "mock".into(), "m".into(), None).unwrap();

    studio.send_message("a", "for-a".into(), "u1".into()).await.unwrap();
    studio.send_message("b", "for-b".into(), "u2".into()).await.unwrap();

    let ta = studio.transcript("a").unwrap();
    let tb = studio.transcript("b").unwrap();
    assert!(ta.iter().any(|m| m.text == "for-a"));
    assert!(!ta.iter().any(|m| m.text == "for-b"), "transcripts must not leak");
    assert!(tb.iter().any(|m| m.text == "for-b"));
    assert_eq!(studio.list_agents().len(), 2);
}

#[test]
fn agent_commands_work_without_a_tokio_runtime_context() {
    // Regression for a real crash: a *synchronous* Tauri command runs with NO
    // tokio runtime context, while dsh spawns the agent driver (and emits
    // disposal events) with a bare `tokio::spawn` — which panics with "there is
    // no reactor running".
    //
    // Every other test in this file is `#[tokio::test]`, so it always has a
    // reactor and could never catch this. A plain `std::thread` reproduces the
    // sync-command situation exactly.
    let dir = tmpdir("plain-thread");
    let studio = tauri::async_runtime::block_on(async {
        Studio::with_hook(None, None, dir).await.unwrap()
    });

    let result = std::thread::spawn(move || {
        // Exercise EVERY studio method a synchronous command can reach, so a
        // future spawn-capable path cannot slip through this test.
        let id = studio.create_agent(
            Some("a1".into()),
            "mock".into(),
            "mock-1".into(),
            Some("/tmp".into()),
        )?;

        // Read-only paths (several sync commands).
        let _ = studio.status();
        studio.list_agents();
        let _ = studio.transcript("a1")?;
        let _ = studio.host().services();
        let _ = studio.host().list_plugins();
        studio.discover();
        studio.watching();
        studio.config();

        // Mutating paths with no spawn (signal-only).
        studio.steer("a1", "hurry".into(), "s1".into())?;
        studio.cancel_agent("a1")?;

        // The one that emits an event (and therefore spawns).
        studio.dispose_agent("a1")?;

        Ok::<String, anyhow::Error>(id)
    })
    .join();

    let id = result.expect("the thread must not panic");
    assert!(id.is_ok(), "agent commands should work off-runtime: {id:?}");
    assert_eq!(id.unwrap(), "a1");
}

// ---------------------------------------------------------------------------
// Frontend UI contributions (plugin-provided slots)
// ---------------------------------------------------------------------------

/// A plugin whose declaration carries a `ui` block.
fn wasm_ui_plugin(slot: &str, json_decl: &str) -> Vec<u8> {
    let decl_off = 64usize;
    let wat = format!(
        r#"(module
          (import "host" "log" (func $log (param i32 i32 i32)))
          (memory (export "memory") 4)
          (global $bump (mut i32) (i32.const 8192))
          (data (i32.const {decl_off}) {decl:?})
          (func (export "plugin_abi_version") (result i32) (i32.const 1))
          (func (export "plugin_init") (result i32) (i32.const 0))
          (func (export "plugin_shutdown"))
          (func (export "plugin_alloc") (param $n i32) (result i32)
            (local $p i32)
            (local.set $p (global.get $bump))
            (global.set $bump (i32.add (global.get $bump) (local.get $n)))
            (local.get $p))
          (func (export "plugin_free") (param i32 i32))
          (func (export "plugin_describe") (param $o i32) (param $c i32) (result i64)
            (local $i i32)
            (if (i32.lt_s (local.get $c) (i32.const {dlen}))
              (then (return (i64.extend_i32_s (i32.sub (i32.const 0) (i32.const {dlen}))))))
            (block $d (loop $l
              (br_if $d (i32.ge_s (local.get $i) (i32.const {dlen})))
              (i32.store8 (i32.add (local.get $o) (local.get $i))
                (i32.load8_u (i32.add (i32.const {decl_off}) (local.get $i))))
              (local.set $i (i32.add (local.get $i) (i32.const 1)))
              (br $l)))
            (i64.const {dlen}))
          (func (export "plugin_invoke")
            (param i32 i32 i32 i32) (param i32 i32) (result i64) (i64.const 0))
        )"#,
        decl_off = decl_off,
        decl = json_decl,
        dlen = json_decl.len(),
    );
    let _ = slot;
    wat::parse_str(&wat).expect("ui plugin wat should parse")
}

#[tokio::test]
async fn a_plugins_declaration_reaches_the_frontend_ui_contract() {
    let dir = tmpdir("ui-decl");
    let decl = r#"{"name":"llm-ui","abi":1,"tools":[],"ui":{
        "provides":[{"name":"llm-ui.config","description":"provider settings"}],
        "injects":[{"slot":"settings.tabs","priority":5,"component":"LlmSettings"}],
        "assets":{"entry.js":"register({});","style.css":".x{}"}}}"#;
    let wasm = dir.join("llm-ui.wasm");
    std::fs::write(&wasm, wasm_ui_plugin("llm-ui", decl)).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .mount_slot("llm-ui", &wasm.display().to_string(), json!(null))
        .await
        .unwrap();

    let ui = studio.host().ui_decls();
    assert_eq!(ui.len(), 1, "one plugin declares UI");
    let (slot, d) = &ui[0];
    assert_eq!(slot, "llm-ui");
    assert_eq!(d.provides.len(), 1);
    assert_eq!(d.provides[0].name, "llm-ui.config");
    assert_eq!(d.injects.len(), 1);
    assert_eq!(d.injects[0].slot, "settings.tabs");
    assert_eq!(d.injects[0].priority, 5);
    assert_eq!(d.injects[0].component.as_deref(), Some("LlmSettings"));
    assert!(d.assets.contains_key("entry.js"));
}

#[tokio::test]
async fn unloading_a_ui_plugin_removes_its_contribution() {
    let dir = tmpdir("ui-unload");
    let decl = r#"{"name":"llm-ui","abi":1,"tools":[],"ui":{"provides":[{"name":"llm-ui.config"}]}}"#;
    let wasm = dir.join("llm-ui.wasm");
    std::fs::write(&wasm, wasm_ui_plugin("llm-ui", decl)).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .mount_slot("llm-ui", &wasm.display().to_string(), json!(null))
        .await
        .unwrap();
    assert_eq!(studio.host().ui_decls().len(), 1);

    studio.unmount_slot("llm-ui").await.unwrap();
    assert!(
        studio.host().ui_decls().is_empty(),
        "an unloaded plugin must not contribute UI"
    );
}

// ---------------------------------------------------------------------------
// End-to-end: two demo plugins, one mounting into the other's slot
// ---------------------------------------------------------------------------

/// Locate a built demo plugin under the sibling repo's target dir.
fn demo_plugin(file: &str) -> PathBuf {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap(); // /Users/yuan
    root.join("wasm-plugin-host/target/wasm32-wasip1/release")
        .join(file)
}

#[tokio::test]
async fn demo_plugins_expose_a_cross_plugin_ui_graph() {
    // A (ui-llm-panel) opens `ui-llm-panel.config` and contributes to
    // `settings.tabs`. B (ui-theme-widget) injects into A's slot.
    let a = demo_plugin("ui_llm_panel.wasm");
    let b = demo_plugin("ui_theme_widget.wasm");
    if !a.exists() || !b.exists() {
        eprintln!("skipping: build the demo plugins first");
        return;
    }

    let dir = tmpdir("e2e-ui");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .mount_slot("ui-llm-panel", &a.display().to_string(), json!(null))
        .await
        .unwrap();
    studio
        .mount_slot("ui-theme-widget", &b.display().to_string(), json!(null))
        .await
        .unwrap();

    let decls = studio.host().ui_decls();
    assert_eq!(decls.len(), 2, "both plugins declare UI");

    let a_decl = decls.iter().find(|(s, _)| s == "ui-llm-panel").unwrap().1.clone();
    let b_decl = decls.iter().find(|(s, _)| s == "ui-theme-widget").unwrap().1.clone();

    // A opens a slot…
    assert_eq!(a_decl.provides.len(), 1);
    let opened = &a_decl.provides[0].name;
    assert_eq!(opened, "ui-llm-panel.config");
    // …and contributes to a built-in one.
    assert_eq!(a_decl.injects[0].slot, "settings.tabs");
    assert!(a_decl.assets.contains_key("entry.js"));

    // B injects into the slot A owns — the cross-plugin link…
    let b_targets: Vec<&str> = b_decl.injects.iter().map(|i| i.slot.as_str()).collect();
    assert!(
        b_targets.contains(&opened.as_str()),
        "B must target the slot A opened; got {b_targets:?}"
    );
    let cross = b_decl
        .injects
        .iter()
        .find(|i| &i.slot == opened)
        .expect("the cross-plugin claim");
    assert_eq!(cross.component.as_deref(), Some("ThemeWidget"));
    // …and also into the built-in tab strip, at a lower priority number than
    // A's, so ui-curator's priority adjustment is observable end to end.
    let strip = b_decl
        .injects
        .iter()
        .find(|i| i.slot == "settings.tabs")
        .expect("B also contributes to the built-in strip");
    let a_strip = a_decl
        .injects
        .iter()
        .find(|i| i.slot == "settings.tabs")
        .expect("A contributes to the built-in strip");
    assert!(
        strip.priority < a_strip.priority,
        "B declares an earlier priority than A, so an adjustment is needed to flip them"
    );
}

#[tokio::test]
async fn a_later_plugin_can_declare_adjustments_to_existing_ui() {
    // `ui-curator` ships no UI of its own; it only reshapes what the other two
    // already contribute. This is the capability the project exists for, so it
    // must survive the whole WASM -> ABI -> command path intact.
    let c = demo_plugin("ui_curator.wasm");
    if !c.exists() {
        return;
    }
    let dir = tmpdir("adjusts");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .mount_slot("ui-curator", &c.display().to_string(), json!(null))
        .await
        .unwrap();

    let decls = studio.host().ui_decls();
    let (_, curator) = decls
        .iter()
        .find(|(s, _)| s == "ui-curator")
        .expect("ui-curator declares UI (its adjustments)");
    assert!(
        curator.injects.is_empty() && curator.provides.is_empty(),
        "the curator only adjusts — it claims nothing of its own"
    );
    assert_eq!(curator.adjusts.len(), 2, "two adjustments declared");

    // An ordering adjustment against another plugin's contribution.
    let ord = curator
        .adjusts
        .iter()
        .find(|a| a.action == wasm_plugin_host::AdjustAction::Priority)
        .expect("a priority adjustment");
    assert_eq!(ord.slot, "settings.tabs");
    assert_eq!(ord.from.as_deref(), Some("ui-llm-panel"));
    assert_eq!(ord.to, Some(-100));

    // A substitution that names the curator's own component.
    let rep = curator
        .adjusts
        .iter()
        .find(|a| a.action == wasm_plugin_host::AdjustAction::Replace)
        .expect("a replace adjustment");
    assert_eq!(rep.from.as_deref(), Some("ui-theme-widget"));
    assert_eq!(rep.component.as_deref(), Some("CuratedPanel"));
}

#[tokio::test]
async fn the_cross_plugin_graph_holds_regardless_of_load_order() {
    let a = demo_plugin("ui_llm_panel.wasm");
    let b = demo_plugin("ui_theme_widget.wasm");
    if !a.exists() || !b.exists() {
        return;
    }

    // Load B FIRST this time.
    let dir = tmpdir("e2e-ui-rev");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .mount_slot("ui-theme-widget", &b.display().to_string(), json!(null))
        .await
        .unwrap();
    studio
        .mount_slot("ui-llm-panel", &a.display().to_string(), json!(null))
        .await
        .unwrap();

    let decls = studio.host().ui_decls();
    let b_decl = decls.iter().find(|(s, _)| s == "ui-theme-widget").unwrap().1.clone();
    let a_decl = decls.iter().find(|(s, _)| s == "ui-llm-panel").unwrap().1.clone();
    // The data is the same; the frontend registry resolves it order-independently
    // (covered by src/lib/plugin-host.test.ts).
    assert_eq!(b_decl.injects[0].slot, a_decl.provides[0].name);
}

#[tokio::test]
async fn list_plugins_surfaces_ui_metadata_for_the_management_page() {
    // The Plugins page must be able to show what UI a plugin brings — not just
    // its tools. This pins the fields it reads.
    let dir = tmpdir("ui-in-list");
    let decl = r#"{"name":"llm-ui","abi":1,"tools":[],"ui":{
        "provides":[{"name":"llm-ui.config"}],
        "injects":[{"slot":"settings.tabs","priority":3,"component":"Panel"}],
        "assets":{"entry.js":"studio.register('Panel', () => {});"}}}"#;
    let wasm = dir.join("llm-ui.wasm");
    std::fs::write(&wasm, wasm_ui_plugin("llm-ui", decl)).unwrap();

    let studio = Studio::with_hook(None, None, dir.clone()).await.unwrap();
    studio
        .mount_slot("llm-ui", &wasm.display().to_string(), json!(null))
        .await
        .unwrap();

    // UI metadata is reachable from the host (used by the command layer).
    let ui = studio.host().ui_decl("llm-ui").expect("ui decl");
    assert_eq!(ui.provides[0].name, "llm-ui.config");
    assert_eq!(ui.injects[0].slot, "settings.tabs");
    assert_eq!(ui.injects[0].component.as_deref(), Some("Panel"));
    assert!(ui.assets.contains_key("entry.js"));

    // And a plugin with no ui block reports none.
    let plain = dir.join("plain.wasm");
    std::fs::write(&plain, wasm_tool("plain")).unwrap();
    studio
        .mount_slot("plain", &plain.display().to_string(), json!(null))
        .await
        .unwrap();
    assert!(
        studio.host().ui_decl("plain").is_none(),
        "a backend-only plugin has no UI declaration"
    );
}

// ---------------------------------------------------------------------------
// Discovery: finding plugins without the user knowing where to look
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discover_finds_wasm_in_the_app_plugin_directory() {
    let dir = tmpdir("disc-app");
    let plugins = dir.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(plugins.join("alpha.wasm"), wasm_tool("alpha")).unwrap();
    std::fs::write(plugins.join("notes.txt"), b"ignore me").unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let d = studio.discover();

    assert_eq!(d.plugins.len(), 1, "only .wasm files count");
    assert_eq!(d.plugins[0].slot, "alpha");
    // The app plugin dir must be among the searched roots, so the UI can point
    // the user at it when nothing is found.
    assert!(
        d.searched
            .iter()
            .any(|r| r.label == "app plugin directory" && r.exists),
        "searched roots: {:?}",
        d.searched
    );
}

#[tokio::test]
async fn discover_reports_every_location_even_when_empty() {
    // The empty case is the one that used to be a dead end. It must now explain
    // itself: which directories were consulted and whether they exist.
    let dir = tmpdir("disc-empty");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let d = studio.discover();

    assert!(d.plugins.is_empty());
    assert!(!d.searched.is_empty(), "must report where it looked");
    let app_root = d
        .searched
        .iter()
        .find(|r| r.label == "app plugin directory")
        .expect("the app plugin directory is always reported");
    assert!(app_root.exists, "the app creates it on boot");
    assert_eq!(app_root.wasm_count, 0);
}

#[tokio::test]
async fn discover_skips_already_configured_plugins() {
    let dir = tmpdir("disc-configured");
    let plugins = dir.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    let wasm = plugins.join("alpha.wasm");
    std::fs::write(&wasm, wasm_tool("alpha")).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .mount_slot("alpha", &wasm.display().to_string(), json!(null))
        .await
        .unwrap();

    let d = studio.discover();
    assert!(
        d.plugins.is_empty(),
        "a configured plugin should not be offered again: {:?}",
        d.plugins
    );
}

// ---------------------------------------------------------------------------
// Catalog: the Plugins list must show every known plugin, running or not
// ---------------------------------------------------------------------------

#[tokio::test]
async fn catalog_lists_a_configured_but_stopped_plugin() {
    // The bug this guards: stopping a plugin used to make it vanish from the
    // list, and discovery would not offer it again either.
    let dir = tmpdir("cat-stopped");
    let wasm = dir.join("alpha.wasm");
    std::fs::write(&wasm, wasm_tool("alpha")).unwrap();
    let path = wasm.display().to_string();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio.mount_slot("alpha", &path, json!(null)).await.unwrap();
    assert_eq!(studio.catalog().len(), 1);

    // Stop it — it stays in the config, so it must stay in the catalog.
    studio.unmount_slot("alpha").await.unwrap();
    let cat = studio.catalog();
    assert_eq!(cat.len(), 1, "a stopped plugin must remain listed: {cat:?}");
    let e = &cat[0];
    assert_eq!(e.slot, "alpha");
    assert!(!e.running, "it is stopped");
    assert!(e.in_config, "still configured");
    assert!(e.exists, "its file is still there");
    assert_eq!(e.state, "stopped");
}

#[tokio::test]
async fn catalog_includes_a_discovered_plugin_never_started() {
    let dir = tmpdir("cat-available");
    let plugins = dir.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(plugins.join("beta.wasm"), wasm_tool("beta")).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let cat = studio.catalog();
    let e = cat.iter().find(|e| e.slot == "beta").expect("discovered plugin listed");
    assert!(!e.running, "not started");
    assert!(!e.in_config, "not configured yet");
    assert_eq!(e.state, "available");
    assert!(e.exists);
}

#[tokio::test]
async fn a_running_plugin_reports_as_running_in_the_catalog() {
    let dir = tmpdir("cat-running");
    let wasm = dir.join("alpha.wasm");
    std::fs::write(&wasm, wasm_tool("alpha")).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .mount_slot("alpha", &wasm.display().to_string(), json!(null))
        .await
        .unwrap();

    let cat = studio.catalog();
    assert_eq!(cat.len(), 1);
    assert!(cat[0].running);
    assert!(cat[0].active);
    assert_eq!(cat[0].tool_count, 1);
}

#[tokio::test]
async fn catalog_does_not_duplicate_a_running_plugin_that_is_also_discoverable() {
    // A started plugin is both "configured" and "on disk" — it must appear once.
    let dir = tmpdir("cat-dedup");
    let plugins = dir.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    let wasm = plugins.join("alpha.wasm");
    std::fs::write(&wasm, wasm_tool("alpha")).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .mount_slot("alpha", &wasm.display().to_string(), json!(null))
        .await
        .unwrap();

    let cat = studio.catalog();
    let alpha_rows = cat.iter().filter(|e| e.slot == "alpha").count();
    assert_eq!(alpha_rows, 1, "no duplicate rows: {cat:?}");
    assert!(cat[0].running);
}

// ---------------------------------------------------------------------------
// Plugin windows (scheme A: the window loads the app and renders a component)
// ---------------------------------------------------------------------------

/// A plugin declaring a `ui.windows` array.
fn wasm_window_plugin(slot: &str, decl: &str) -> Vec<u8> {
    wasm_ui_plugin(slot, decl)
}

#[tokio::test]
async fn a_plugin_can_declare_a_window() {
    let dir = tmpdir("win-decl");
    let decl = r#"{"name":"llm-ui","abi":1,"tools":[],"ui":{
        "assets":{"entry.js":"studio.register('Adv', () => {});"},
        "windows":[{
            "name":"advanced","component":"Adv","title":"Advanced",
            "width":640,"height":480,"open":"manual"
        }]}}"#;
    let wasm = dir.join("llm-ui.wasm");
    std::fs::write(&wasm, wasm_window_plugin("llm-ui", decl)).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .mount_slot("llm-ui", &wasm.display().to_string(), json!(null))
        .await
        .unwrap();

    let wins = studio.plugin_windows();
    assert_eq!(wins.len(), 1, "one window declared");
    let w = &wins[0];
    // The label is namespaced by the slot, so two plugins cannot collide.
    assert_eq!(w.label, "plugin-llm-ui-advanced");
    assert_eq!(w.slot, "llm-ui");
    assert_eq!(w.component, "Adv");
    assert_eq!(w.title, "Advanced");
    assert_eq!(w.width, 640.0);
    assert_eq!(w.height, 480.0);
    assert_eq!(w.open, "manual");
}

#[tokio::test]
async fn a_window_is_looked_up_by_its_own_label() {
    // This is what a plugin window does on startup: it knows only its label.
    let dir = tmpdir("win-lookup");
    let decl = r#"{"name":"p","abi":1,"tools":[],"ui":{
        "windows":[{"name":"panel","component":"Panel"}]}}"#;
    let wasm = dir.join("p.wasm");
    std::fs::write(&wasm, wasm_window_plugin("p", decl)).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .mount_slot("p", &wasm.display().to_string(), json!(null))
        .await
        .unwrap();

    let found = studio.plugin_window_by_label("plugin-p-panel");
    assert!(found.is_some(), "lookup by derived label works");
    assert_eq!(found.unwrap().component, "Panel");

    // An unknown label is None, not a panic — that is the main window's case.
    assert!(studio.plugin_window_by_label("main").is_none());
}

#[tokio::test]
async fn two_plugins_may_use_the_same_window_name_without_collision() {
    let dir = tmpdir("win-namespace");
    let decl_a = r#"{"name":"a","abi":1,"tools":[],"ui":{
        "windows":[{"name":"settings","component":"A"}]}}"#;
    let decl_b = r#"{"name":"b","abi":1,"tools":[],"ui":{
        "windows":[{"name":"settings","component":"B"}]}}"#;
    let wa = dir.join("a.wasm");
    let wb = dir.join("b.wasm");
    std::fs::write(&wa, wasm_window_plugin("a", decl_a)).unwrap();
    std::fs::write(&wb, wasm_window_plugin("b", decl_b)).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio.mount_slot("a", &wa.display().to_string(), json!(null)).await.unwrap();
    studio.mount_slot("b", &wb.display().to_string(), json!(null)).await.unwrap();

    let wins = studio.plugin_windows();
    assert_eq!(wins.len(), 2);
    let labels: Vec<&str> = wins.iter().map(|w| w.label.as_str()).collect();
    assert!(labels.contains(&"plugin-a-settings"));
    assert!(labels.contains(&"plugin-b-settings"));
}

#[tokio::test]
async fn stopping_a_plugin_removes_its_windows_from_the_list() {
    let dir = tmpdir("win-stop");
    let decl = r#"{"name":"p","abi":1,"tools":[],"ui":{
        "windows":[{"name":"panel","component":"Panel"}]}}"#;
    let wasm = dir.join("p.wasm");
    std::fs::write(&wasm, wasm_window_plugin("p", decl)).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio.mount_slot("p", &wasm.display().to_string(), json!(null)).await.unwrap();
    assert_eq!(studio.plugin_windows().len(), 1);

    studio.unmount_slot("p").await.unwrap();
    assert!(
        studio.plugin_windows().is_empty(),
        "a stopped plugin declares no windows"
    );
}

#[tokio::test]
async fn opening_an_unknown_window_is_a_clear_error_not_a_panic() {
    // Headless (no app handle) and unknown label must both be errors, never a
    // panic — the command layer turns them into a message for the UI.
    let dir = tmpdir("win-unknown");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let err = studio.open_plugin_window("plugin-nope-x").unwrap_err();
    assert!(
        err.to_string().contains("no plugin declares a window"),
        "got: {err}"
    );
}

#[tokio::test]
async fn window_labels_are_sanitised_for_tauri() {
    // Tauri labels allow only alphanumerics plus - / : _ ; a plugin name with
    // other characters must not produce an invalid label.
    assert_eq!(
        Studio::window_label("my.plugin", "a b/c"),
        "plugin-my-plugin-a-b-c"
    );
}

// ---------------------------------------------------------------------------
// Scheme C: self-contained HTML windows, and window params
// ---------------------------------------------------------------------------

/// A plugin declaring an `html`-content window.
fn html_window_decl() -> &'static str {
    r#"{"name":"p","abi":1,"tools":[],"ui":{
        "windows":[{
            "name":"page","component":"unused","title":"Standalone",
            "content":"html",
            "html":"<h1 id=hi>standalone page</h1><script>document.title='set-by-plugin'</script>"
        }]}}"#
}

#[tokio::test]
async fn a_plugin_can_declare_a_self_contained_html_window() {
    let dir = tmpdir("win-html");
    let wasm = dir.join("p.wasm");
    std::fs::write(&wasm, wasm_ui_plugin("p", html_window_decl())).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio.mount_slot("p", &wasm.display().to_string(), json!(null)).await.unwrap();

    let w = &studio.plugin_windows()[0];
    assert_eq!(w.content, "html", "content mode is surfaced to the UI");
    // The html itself is reachable for the injection step.
    let html = studio.window_html("plugin-p-page").expect("html present");
    assert!(html.contains("standalone page"));
}

#[tokio::test]
async fn an_app_window_reports_content_app_and_has_no_html() {
    let dir = tmpdir("win-app-content");
    let decl = r#"{"name":"p","abi":1,"tools":[],"ui":{
        "windows":[{"name":"panel","component":"Panel"}]}}"#;
    let wasm = dir.join("p.wasm");
    std::fs::write(&wasm, wasm_ui_plugin("p", decl)).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio.mount_slot("p", &wasm.display().to_string(), json!(null)).await.unwrap();

    let w = &studio.plugin_windows()[0];
    assert_eq!(w.content, "app", "default content mode is app");
    assert!(studio.window_html("plugin-p-panel").is_none(), "no html for app windows");
}

#[test]
fn the_html_window_bootstrap_escapes_its_payload() {
    // The injected script must survive HTML containing quotes, backslashes and
    // newlines. A naive `"{}"` interpolation would break out of the JS string
    // literal; JSON encoding is what makes this safe, so the test asserts the
    // *exact* JSON-encoded payload appears.
    let nasty = "<div title=\"a\\\"b\">line1\nline2 \\ backslash</div>";
    let script = dsh_wasm_studio_lib::studio::html_window_bootstrap(nasty, "plugin-x-y");

    // The payload must appear exactly as serde_json encodes it.
    let encoded = serde_json::to_string(nasty).unwrap();
    assert!(
        script.contains(&encoded),
        "the HTML must be JSON-encoded, not raw.\nexpected to find: {encoded}\nin: {script}"
    );
    // A raw newline inside the literal would be a syntax error.
    let start = script.find(&encoded).unwrap();
    let payload = &script[start..start + encoded.len()];
    assert!(!payload.contains('\n'), "no raw newline may appear in the literal");
    assert!(script.contains("document.write"), "writes the document");
    assert!(script.contains("plugin-x-y"), "carries the label");
}

#[test]
fn the_app_window_bootstrap_carries_label_and_params() {
    let script = dsh_wasm_studio_lib::studio::app_window_bootstrap("plugin-p-panel", r#"{"id":42}"#);
    assert!(script.contains("plugin-p-panel"));
    assert!(script.contains("42"));
    assert!(script.contains("__STUDIO_WINDOW__"));
}

#[tokio::test]
async fn params_are_queued_until_the_window_opens() {
    let dir = tmpdir("win-params");
    let decl = r#"{"name":"p","abi":1,"tools":[],"ui":{
        "windows":[{"name":"panel","component":"Panel"}]}}"#;
    let wasm = dir.join("p.wasm");
    std::fs::write(&wasm, wasm_ui_plugin("p", decl)).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio.mount_slot("p", &wasm.display().to_string(), json!(null)).await.unwrap();

    // Queue params, then read them back (headless has no window to open).
    studio.queue_window_params("plugin-p-panel", json!({ "sel": "x" }));
    let got = studio.peek_pending_params("plugin-p-panel");
    assert_eq!(got, Some(json!({ "sel": "x" })));
    // Taking delivers and clears exactly once.
    assert_eq!(studio.take_pending_params("plugin-p-panel"), Some(json!({ "sel": "x" })));
    assert_eq!(studio.take_pending_params("plugin-p-panel"), None);
}

// ---------------------------------------------------------------------------
// Bounded fiber joins
//
// cordis's `Fiber::join` has a lost-wakeup race (it consumes the settle
// notification with `borrow_and_update` and then awaits the *next* one), which
// shows up as an intermittent hang in a fresh process. The studio therefore
// never calls the unbounded form.
// ---------------------------------------------------------------------------

/// A slot whose inject no one provides must still load.
///
/// **What this does and does not prove.** It pins the *semantics* the bounded
/// join relies on: a plugin held back by an unsatisfied inject is a legitimate
/// state, not a mount failure, and it stays inactive. It does **not** reproduce
/// cordis's lost-wakeup race — a pending fiber has already settled (`dirty`
/// false, no live task), so even the unbounded `join()` returns at once.
/// The race needs a *settling* fiber and a thread switch inside a narrow window;
/// it is what `join_bounded` guards against, and it was reproduced out of band
/// (~1 in 10 fresh processes, multi-thread runtime only).
#[tokio::test]
async fn a_slot_that_cannot_settle_still_loads() {
    let dir = tmpdir("bounded-join");
    // This plugin injects a service nothing provides, so it stays pending
    // forever. With an unbounded join the mount below would hang.
    let wasm = dir.join("waiter.wasm");
    // Declares a *service* inject nothing provides, so the fiber never settles.
    std::fs::write(
        &wasm,
        wasm_ui_plugin(
            "waiter",
            r#"{"name":"waiter","abi":1,"tools":[],"injects":["nobody-provides-this"]}"#,
        ),
    )
    .unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let mounted = studio
        .mount_slot("waiter", &wasm.display().to_string(), json!(null))
        .await;
    assert!(
        mounted.is_ok(),
        "a pending slot is a legitimate state, not a mount failure: {mounted:?}"
    );
    let entry = studio
        .catalog()
        .into_iter()
        .find(|e| e.slot == "waiter")
        .expect("it is listed");
    assert!(entry.running, "the guest is mounted");
    // `(slot, plugin, state, tools, active)` — an unsatisfied inject means its
    // effects are not registered, so `active` must be false.
    let (_, _, _, tool_count, active) = studio
        .host()
        .list_plugins()
        .into_iter()
        .find(|(s, ..)| s == "waiter")
        .expect("the slot is known to the host");
    assert!(!active, "an unsatisfied inject leaves the plugin inactive");
    assert_eq!(tool_count, 0);
}


// ---------------------------------------------------------------------------
// Startup window: which window the app opens *instead of* its default view
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plugin_can_declare_the_startup_window() {
    let dir = tmpdir("startup-decl");
    let decl = r#"{"name":"p","abi":1,"tools":[],"ui":{
        "assets":{"entry.js":"studio.register('Board', () => {});"},
        "windows":[
          {"name":"board","component":"Board","open":"startup"},
          {"name":"side","component":"Board","open":"manual"}
        ]}}"#;
    let wasm = dir.join("p.wasm");
    std::fs::write(&wasm, wasm_ui_plugin("p", decl)).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio.mount_slot("p", &wasm.display().to_string(), json!(null)).await.unwrap();

    let sw = studio
        .startup_window()
        .expect("one startup window is not a conflict")
        .expect("a startup window is declared");
    assert_eq!(sw.name, "board");
    assert_eq!(sw.label, "plugin-p-board");
    assert_eq!(sw.open, "startup");
}

#[tokio::test]
async fn auto_windows_are_attempted_at_mount_time() {
    // Regression: `boot` installed the app handle *after* `with_hook` had
    // already autoloaded plugins (and thus tried to open their `open:"auto"`
    // windows). The handle was still None, so every auto window silently
    // failed with "no app handle". Headless, we cannot create a window — but we
    // CAN assert the attempt was made, which is what was missing.
    let dir = tmpdir("auto-attempt");
    let decl = r#"{"name":"p","abi":1,"tools":[],"ui":{
        "assets":{"entry.js":"studio.register('X', () => {});"},
        "windows":[{"name":"dash","component":"X","open":"auto"}]}}"#;
    let wasm = dir.join("p.wasm");
    std::fs::write(&wasm, wasm_ui_plugin("p", decl)).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio.mount_slot("p", &wasm.display().to_string(), json!(null)).await.unwrap();

    // Headless there is no window to open, but the request must have been
    // recorded — that is the record the startup path replays once a real app
    // handle exists.
    let wants = studio.windows_wanted_at_mount();
    assert_eq!(
        wants,
        vec!["plugin-p-dash".to_string()],
        "an auto window must be recorded at mount time, even headless"
    );
}

#[tokio::test]
async fn two_plugins_cannot_both_own_the_startup_window() {
    // "Which window starts" has exactly one answer. Silently picking one (by
    // load order) would leave the other plugin's author with a result they
    // cannot explain — the same reason a duplicate slot name is an error.
    let dir = tmpdir("startup-conflict");
    let decl = |name: &str| {
        format!(
            r#"{{"name":"{name}","abi":1,"tools":[],"ui":{{
                "assets":{{"entry.js":"studio.register('W', () => {{}});"}},
                "windows":[{{"name":"main-w","component":"W","open":"startup"}}]}}}}"#
        )
    };
    let a = dir.join("a.wasm");
    let b = dir.join("b.wasm");
    std::fs::write(&a, wasm_ui_plugin("a", &decl("a"))).unwrap();
    std::fs::write(&b, wasm_ui_plugin("b", &decl("b"))).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio.mount_slot("a", &a.display().to_string(), json!(null)).await.unwrap();
    studio.mount_slot("b", &b.display().to_string(), json!(null)).await.unwrap();

    let err = studio.startup_window().unwrap_err().to_string();
    assert!(err.contains("startup window"), "unexpected error: {err}");
    assert!(err.contains("plugin-a-main-w") && err.contains("plugin-b-main-w"),
        "the error must name both claimants so it is actionable, got: {err}");
}

#[tokio::test]
async fn a_plugin_that_only_uses_auto_does_not_claim_the_startup_window() {
    // The two are different: `auto` opens whenever the plugin activates, and
    // must never be mistaken for "own the launch view".
    let dir = tmpdir("auto-not-startup");
    let decl = r#"{"name":"p","abi":1,"tools":[],"ui":{
        "assets":{"entry.js":"studio.register('X', () => {});"},
        "windows":[{"name":"dash","component":"X","open":"auto"}]}}"#;
    let wasm = dir.join("p.wasm");
    std::fs::write(&wasm, wasm_ui_plugin("p", decl)).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio.mount_slot("p", &wasm.display().to_string(), json!(null)).await.unwrap();

    assert!(
        studio.startup_window().unwrap().is_none(),
        "an `auto` window is not a startup window"
    );
}

#[test]
fn an_app_windows_url_targets_the_plugin_window_route() {
    // Regression: the URL used to be `index.html?plugin-window=<label>`. The SPA
    // routes on *pathname*, so that matched the `[...plugin]` catch-all; the
    // plugin-window route never mounted, `plugin_window_for` was never called,
    // and the window came up **blank** — while every headless test passed, since
    // they checked the backend lookup and never the URL.
    let path = Studio::app_window_path("plugin-p-board");

    let route = path.split('?').next().unwrap();
    assert_eq!(
        route, "plugin-window",
        "the pathname must be the route that renders plugin components"
    );
    assert!(
        !route.ends_with(".html"),
        "a file URL would be resolved by the static handler, not the router"
    );
    assert!(
        path.contains("label=plugin-p-board"),
        "the window still needs its label: {path}"
    );
}

// ---------------------------------------------------------------------------
// The shell's slot surface: a plugin opens slots, another plugin fills them
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_shells_slots_are_visible_to_later_plugins() {
    // The `hana-shell` design rests on this: the shell declares its slots, and
    // a plugin written later contributes to them without knowing anything about
    // the shell. If `provides` did not reach the frontend, every such
    // contribution would silently vanish — a feature-shaped hole with no error.
    let dir = tmpdir("shell-slots");
    let shell = r#"{"name":"shell","abi":1,"tools":[],"ui":{
        "assets":{"entry.js":"studio.register('S', () => {});"},
        "provides":[
          {"name":"hana.sidebar.sessions","description":"the session list"},
          {"name":"hana.rail.items","description":"right column body"}
        ],
        "windows":[{"name":"main","component":"S","open":"startup"}]}}"#;
    let wasm = dir.join("shell.wasm");
    std::fs::write(&wasm, wasm_ui_plugin("shell", shell)).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio.mount_slot("shell", &wasm.display().to_string(), json!(null)).await.unwrap();

    let decls = studio.host().ui_decls();
    let provided: Vec<String> = decls
        .iter()
        .flat_map(|(_, ui)| ui.provides.iter().map(|p| p.name.clone()))
        .collect();
    assert!(
        provided.contains(&"hana.sidebar.sessions".to_string()),
        "the shell's slots must be declared, got {provided:?}"
    );
    assert_eq!(provided.len(), 2, "both slots, no more: {provided:?}");
}

#[tokio::test]
async fn a_later_plugin_may_fill_a_slot_it_did_not_open() {
    // Order-independence is the property that makes the slot system usable: a
    // plugin may be loaded before the shell that opens the slot it wants, so the
    // contribution has to be *remembered* rather than dropped.
    let dir = tmpdir("shell-inject-later");
    let addon = r#"{"name":"addon","abi":1,"tools":[],"ui":{
        "assets":{"entry.js":"studio.register('A', () => {});"},
        "injects":[{"slot":"hana.sidebar.sessions","component":"A","priority":10}]}}"#;
    let wasm = dir.join("addon.wasm");
    std::fs::write(&wasm, wasm_ui_plugin("addon", addon)).unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    // Mount the *contributor* first — the slot it targets does not exist yet.
    studio.mount_slot("addon", &wasm.display().to_string(), json!(null)).await.unwrap();

    let decls = studio.host().ui_decls();
    let injects: Vec<String> = decls
        .iter()
        .flat_map(|(_, ui)| ui.injects.iter().map(|i| i.slot.clone()))
        .collect();
    assert_eq!(
        injects,
        vec!["hana.sidebar.sessions".to_string()],
        "the contribution must survive even though its slot is not open yet"
    );
}

#[tokio::test]
async fn the_shell_slots_a_panel_mounts_are_the_ones_it_declares() {
    // The shell declares its slots in Rust (`provides`) AND mounts them in JS
    // (`studio.renderSlot`). Those two lists can drift, and a slot that is
    // declared but never mounted is a contribution that goes nowhere — an
    // invisible failure, which is exactly why it needs a test.
    //
    // The check is on the *shipped manifest*: load the real `hana-shell.wasm`
    // if it has been built, and assert every declared slot is a `hana.` name.
    // (Skipped when the plugin has not been built yet, so a fresh clone still
    // passes — the plugin is a demo, not a dependency.)
    let built = std::path::Path::new(
        "/Users/yuan/wasm-plugin-host/target/wasm32-wasip1/release/hana_shell.wasm",
    );
    if !built.exists() {
        eprintln!("skipping: hana-shell wasm not built");
        return;
    }
    let dir = tmpdir("shell-manifest");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .mount_slot("shell", &built.display().to_string(), json!(null))
        .await
        .unwrap();

    let decls = studio.host().ui_decls();
    let (_, ui) = &decls[0];
    let provided: Vec<String> = ui.provides.iter().map(|p| p.name.clone()).collect();

    assert!(
        provided.len() >= 10,
        "the shell must open a slot per layout region, found {}: {provided:?}",
        provided.len()
    );
    for name in &provided {
        assert!(
            name.starts_with("hana."),
            "slot names stay in the shell's namespace: {name}"
        );
    }
    // The regions a plugin is most likely to want must be present, since the
    // names are the shell's public contract.
    for required in [
        "hana.titlebar.right",
        "hana.sidebar.sessions",
        "hana.sidebar.footer",
        "hana.conversation.input.dock",
        "hana.rail.items",
        "hana.shell.overlay",
    ] {
        assert!(
            provided.iter().any(|p| p == required),
            "`{required}` is part of the contract but was not declared"
        );
    }

    // **The declared list and the mounted list must be the same set.** A slot
    // that is declared but never mounted is a contribution that goes nowhere —
    // no error, no log, just a plugin that appears to do nothing. This is the
    // failure the test is named for, so it checks the JS, not just the manifest:
    // every `S.mount('hana.…')` in the shipped assets must correspond to a
    // declared slot, and vice versa.
    //
    // (Found by exactly this check: `hana.rail.header` was declared in the Rust
    // manifest but the rail panel never mounted it. Reading only the manifest
    // would have missed it.)
    let js: String = ui
        .assets
        .iter()
        .filter(|(k, _)| k.ends_with(".js"))
        .map(|(_, v)| v.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let mut mounted: Vec<String> = Vec::new();
    for marker in ["S.mount('", "studio.renderSlot('"] {
        let mut rest = js.as_str();
        while let Some(i) = rest.find(marker) {
            rest = &rest[i + marker.len()..];
            if let Some(end) = rest.find('\'') {
                let name = &rest[..end];
                if name.starts_with("hana.") && !mounted.iter().any(|m| m == name) {
                    mounted.push(name.to_string());
                }
            }
        }
    }

    for name in &provided {
        assert!(
            mounted.iter().any(|m| m == name),
            "`{name}` is declared in the manifest but no panel mounts it — a \
             contribution to it would silently go nowhere. mounted: {mounted:?}"
        );
    }
    for name in &mounted {
        assert!(
            provided.iter().any(|p| p == name),
            "`{name}` is mounted by a panel but not declared — a later plugin \
             cannot target a slot the host does not know about. declared: {provided:?}"
        );
    }
}

#[tokio::test]
async fn the_shell_ships_every_module_its_entry_requires() {
    // Regression: `lib/motion.js` existed on disk but was missing from the
    // plugin's `assets`, so the host never received it and
    // `studio.require('lib/motion')` threw at load — the whole shell rendered
    // nothing. `require` only sees assets that were declared, so a file that
    // exists but is not declared is invisible in a way nothing else catches.
    let built = std::path::Path::new(
        "/Users/yuan/wasm-plugin-host/target/wasm32-wasip1/release/hana_shell.wasm",
    );
    if !built.exists() {
        eprintln!("skipping: hana-shell wasm not built");
        return;
    }
    let dir = tmpdir("shell-assets");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .mount_slot("shell", &built.display().to_string(), json!(null))
        .await
        .unwrap();

    let decls = studio.host().ui_decls();
    let assets: Vec<String> = decls
        .iter()
        .flat_map(|(_, ui)| ui.assets.keys().cloned())
        .collect();

    // Every module `entry.js` reaches for must be present, transitively: the
    // panels require siblings too.
    for required in ["lib/tokens.js", "lib/motion.js", "lib/dom.js", "lib/slots.js", "lib/resize.js"] {
        assert!(
            assets.iter().any(|a| a == required),
            "`{required}` is required by the shell but not among its assets: {assets:?}"
        );
    }
    assert!(
        assets.iter().any(|a| a == "style.css"),
        "the stylesheet carries the whole look and must ship: {assets:?}"
    );
    assert!(assets.iter().any(|a| a == "entry.js"), "entry.js must ship");
}

// ---------------------------------------------------------------------------
// The agent list: a readable title, and token usage read from the session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_agent_list_carries_a_readable_title_and_usage() {
    use dsh_rs::api::services::LlmService;
    use dsh_rs::types::{FinishReason, StreamChunk, TokenUsage};
    use std::sync::Arc;

    // The shell's session list renders `AgentRow` directly, so an agent id
    // ("a1") with no title is a list of generated strings nobody recognises.
    // The title is derived from the first user message.
    //
    // **Usage must arrive BEFORE `Finish`.** The agent loop breaks on the first
    // `Finish` chunk, so a provider that reports usage after it is never heard —
    // including dsh's own `MockAdapter::with_usage()`, which appends the usage
    // chunk to the end. Scripting the chunks ourselves is therefore the only way
    // to exercise the extraction, and it documents the ordering it requires.
    let dir = tmpdir("agent-row");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();

    let runtime = studio
        .ctx()
        .require::<LlmService>(dsh_rs::api::LLM_SERVICE)
        .unwrap();
    runtime.unregister_adapter(&["mock"]);
    runtime
        .register_adapter(
            &["mock"],
            Arc::new(dsh_rs::llm::adapters::mock::MockAdapter::scripted(vec![vec![
                StreamChunk::BlockStart { index: 0, block_type: "text".into() },
                StreamChunk::TextDelta { index: 0, text: "ok".into() },
                StreamChunk::BlockEnd {
                    index: 0,
                    block: dsh_rs::types::ContentBlock::Text { text: "ok".into() },
                },
                StreamChunk::Usage {
                    usage: TokenUsage {
                        input_tokens: 12,
                        output_tokens: 7,
                        ..Default::default()
                    },
                },
                StreamChunk::Finish { reason: FinishReason::Stop },
            ]])),
        )
        .unwrap();

    studio
        .create_agent(Some("a1".into()), "mock".into(), "mock-1".into(), Some("/tmp".into()))
        .expect("agent created");

    // Before any message there is nothing to derive a title from, and the UI
    // shows a placeholder rather than an empty row.
    let before = studio.list_agents();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].title, "", "no user message yet, so no title");
    assert_eq!(before[0].usage.calls, 0, "no calls yet");

    studio
        .send_message("a1", "refactor the token estimator".into(), "u1".into())
        .await
        .expect("turn completes");

    let after = studio.list_agents();
    assert_eq!(
        after[0].title, "refactor the token estimator",
        "the title is the first user message"
    );
    assert_eq!(after[0].usage.calls, 1, "one assistant message carried usage");
    assert_eq!(after[0].usage.input, 12, "input tokens are summed");
    assert_eq!(after[0].usage.output, 7, "output tokens are summed");
    // The optional fields were not reported, so they stay absent rather than
    // becoming a misleading zero.
    assert_eq!(after[0].usage.cache_read, None);
}

// A provider that reports usage after `Finish` is inaudible to the loop — the
// ordering trap above, asserted so the behaviour is a fact rather than folklore.
#[tokio::test]
async fn usage_after_finish_is_invisible_to_the_loop() {
    use dsh_rs::api::services::LlmService;
    use std::sync::Arc;

    let dir = tmpdir("agent-usage-late");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let runtime = studio
        .ctx()
        .require::<LlmService>(dsh_rs::api::LLM_SERVICE)
        .unwrap();
    runtime.unregister_adapter(&["mock"]);
    // dsh's own `with_usage()` appends Usage to the end of `text_response`,
    // i.e. *after* Finish. This asserts the consequence, which is why the shell
    // must render "no usage reported" rather than "0 tokens".
    runtime
        .register_adapter(
            &["mock"],
            Arc::new(dsh_rs::llm::adapters::mock::MockAdapter::new().with_usage()),
        )
        .unwrap();

    studio
        .create_agent(Some("a1".into()), "mock".into(), "mock-1".into(), Some("/tmp".into()))
        .expect("agent created");
    studio
        .send_message("a1", "hi".into(), "u1".into())
        .await
        .expect("turn completes");

    let row = &studio.list_agents()[0];
    assert_eq!(
        row.usage.calls, 0,
        "a usage chunk after Finish never reaches the session, even from dsh's \
         own mock; if this starts failing, dsh fixed the ordering"
    );
    assert_eq!(row.usage.cache_read, None, "and nothing is invented to fill it");
}

#[tokio::test]
async fn usage_is_absent_when_the_provider_does_not_report_it() {
    // The default mock reports nothing, and so would any gateway that omits the
    // usage block. The distinction matters: "no tokens reported" must not render
    // as "0 tokens used", which reads as a real measurement.
    let dir = tmpdir("agent-usage-none");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .create_agent(Some("a1".into()), "mock".into(), "mock-1".into(), Some("/tmp".into()))
        .expect("agent created");
    studio
        .send_message("a1", "hello".into(), "u1".into())
        .await
        .expect("turn completes");

    let row = &studio.list_agents()[0];
    assert_eq!(row.usage.calls, 0, "nothing reported usage");
    assert_eq!(row.usage.input, 0);
    // The optional fields stay absent rather than becoming a misleading zero.
    assert_eq!(row.usage.cache_read, None, "no cache figure to claim");
    assert_eq!(row.usage.cache_write, None);
    assert_eq!(row.usage.reasoning, None);
}

#[tokio::test]
async fn a_long_first_message_is_truncated_by_characters_not_bytes() {
    // Truncation by byte would split a multi-byte codepoint and produce invalid
    // UTF-8 (or panic). Using 4-byte characters makes the difference visible:
    // 60 chars is 240 bytes.
    let dir = tmpdir("agent-title-i18n");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .create_agent(Some("a1".into()), "mock".into(), "mock-1".into(), Some("/tmp".into()))
        .expect("agent created");

    let long: String = "汉".repeat(200);
    studio
        .send_message("a1", long.clone(), "u1".into())
        .await
        .expect("turn completes");

    let title = studio.list_agents()[0].title.clone();
    assert!(title.ends_with('…'), "a truncated title is marked: {title}");
    // 60 kept characters plus the ellipsis. If this were byte-based the string
    // would be mangled rather than merely long.
    assert_eq!(title.chars().count(), 61, "60 chars + ellipsis: {title}");
    assert!(title.starts_with('汉'), "the kept prefix is intact: {title}");
    assert!(title.is_char_boundary(title.len()), "still valid UTF-8");
}

#[tokio::test]
async fn status_always_reports_the_mock_route() {
    // The shell's chat picks a provider from `status().providers`, and treats an
    // empty list as "no LLM configured" — which would leave the composer unable
    // to send anything. dsh's base bundle always registers `mock`, so the list
    // must never be empty: if this fails, the shell would wrongly report "no
    // provider" on a perfectly healthy boot.
    let dir = tmpdir("status-providers");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let providers = studio.status().providers;
    assert!(
        providers.iter().any(|p| p == "mock"),
        "the base bundle registers `mock`, so it must be reported: {providers:?}"
    );
}

#[tokio::test]
async fn a_session_is_written_to_disk() {
    // dsh's JSONL persistence is attached via `BaseConfig::store_dir`. This
    // asserts the write half: a turn's events reach a file under the app-data
    // directory, so nothing is lost when the process exits.
    //
    // The *read* half is separate — `attach_persistence` only adds a write
    // backend, so the store does not reload what is on disk. dsh's own CLI
    // reloads explicitly (`backend.list()` then `backend.load(id)`), and the
    // studio does not do that yet.
    let dir = tmpdir("session-persist");
    let studio = Studio::with_hook(None, None, dir.clone()).await.unwrap();

    studio
        .create_agent(Some("p1".into()), "mock".into(), "mock-1".into(), Some("/tmp".into()))
        .expect("agent created");
    studio
        .send_message("p1", "persist me".into(), "u1".into())
        .await
        .expect("turn completes");

    let sessions = dir.join("sessions");
    let mut found: Option<std::path::PathBuf> = None;
    if let Ok(entries) = std::fs::read_dir(&sessions) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().ends_with(".jsonl") {
                found = Some(e.path());
            }
        }
    }
    let path = found.unwrap_or_else(|| {
        panic!("no session jsonl under {} (was store_dir wired up?)", sessions.display())
    });

    let body = std::fs::read_to_string(&path).unwrap();
    assert!(
        body.contains("persist me"),
        "the user message must be on disk: {}",
        path.display()
    );
    // The event *type* is kebab-case on the wire (`turn-start`), not the
    // slash-form the cordis event carries (`turn/start`) — asserting the wrong
    // one would pass on the text alone and miss a half-written log.
    assert!(
        body.contains("\"type\":\"turn-start\""),
        "the whole event stream is recorded, not just the message text: {}",
        path.display()
    );
    assert!(
        body.contains("\"type\":\"turn-end\""),
        "the turn is closed on disk, so a reload sees a complete turn: {}",
        path.display()
    );
}

#[tokio::test]
async fn a_configured_provider_reaches_the_llm_seam() {
    // `base_config` copies `extra.llm.providers` into dsh's `BaseConfig::adapters`,
    // and the bundle mounts each entry as a provider route. This is what makes a
    // configured endpoint *reachable* rather than silently ignored — the bug the
    // shell's provider picker was written to stop hiding.
    //
    // Asserted by writing a real `studio.json` before boot and reading the routes
    // back off the seam, so it exercises the whole path (config file → base
    // config → adapters → registered routes), not just the translation helper.
    let dir = tmpdir("provider-config");
    let cfg_path = dir.join("studio.json");
    std::fs::write(
        &cfg_path,
        json!({
            "extra": {
                "llm": {
                    "providers": {
                        "deepseek": {
                            "base_url": "https://api.deepseek.com/v1",
                            "api_key": "test-key",
                            "model": "deepseek-chat"
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let providers = studio.status().providers;
    assert!(
        providers.iter().any(|p| p == "deepseek"),
        "a configured provider must be registered as a route: {providers:?}"
    );
    assert!(
        providers.iter().any(|p| p == "mock"),
        "and the base bundle's mock is still there: {providers:?}"
    );
}

// ---------------------------------------------------------------------------
// A: reading stored sessions; B: resuming them
// ---------------------------------------------------------------------------

/// Build a session on disk the way a previous run would have left one.
fn seeded_session(dir: &std::path::Path, id: &str, user_text: &str) {
    use dsh_rs::types::{ContentBlock, Message, SessionEvent, SessionEventData, TurnEndReason};
    let msg = Message::user(
        "u1",
        vec![ContentBlock::Text { text: user_text.to_string() }],
    );
    let events = [
        SessionEvent::new(0, 1, SessionEventData::TurnStart { turn: 1 }),
        SessionEvent::new(1, 2, SessionEventData::UserMessage { message: msg }),
        SessionEvent::new(2, 3, SessionEventData::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        }),
    ];
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let body: String = events
        .iter()
        .map(|e| serde_json::to_string(e).unwrap() + "\n")
        .collect();
    std::fs::write(sessions.join(format!("{id}.jsonl")), body).unwrap();
}

#[tokio::test]
async fn a_restart_lists_the_sessions_that_were_written_to_disk() {
    // The whole point of persistence: after a restart the sidebar shows the
    // conversation even though nothing has it in memory.
    let dir = tmpdir("list-stored");
    seeded_session(&dir, "old-1", "what did we decide about the parser");

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    assert!(
        studio.list_agents().is_empty(),
        "a fresh process has no live agents"
    );

    let rows = studio.list_sessions();
    let row = rows
        .iter()
        .find(|r| r.id == "old-1")
        .expect("the stored session must be listed");
    assert!(!row.live, "a session with no driver behind it is not live: {row:?}");
    assert_eq!(row.status, "stored");
    assert_eq!(row.updated_at, Some(3), "latest event time drives sidebar age");
    assert_eq!(
        row.title, "what did we decide about the parser",
        "the title comes from the stored first user message"
    );
}

#[tokio::test]
async fn a_stored_session_is_not_listed_twice_once_it_is_live() {
    // A restored session is live *and* on disk. Reporting it as both would show
    // the user two rows for one conversation, one of them read-only.
    let dir = tmpdir("no-double-list");
    seeded_session(&dir, "old-1", "hello there");

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio.resume_session("old-1").unwrap();

    let rows = studio.list_sessions();
    let matching: Vec<_> = rows.iter().filter(|r| r.id == "old-1").collect();
    assert_eq!(matching.len(), 1, "exactly one row: {rows:?}");
    assert!(matching[0].live, "and it is the live one");
}

#[tokio::test]
async fn resuming_a_session_restores_its_history_and_can_be_continued() {
    // B's acceptance test: the restored agent can actually run a turn, and the
    // reply lands on top of the old history rather than replacing it.
    let dir = tmpdir("resume");
    seeded_session(&dir, "old-1", "first question");

    let studio = Studio::with_hook(None, None, dir).await.unwrap();

    // Before resuming: readable, but there is no agent to send to.
    let before = studio.transcript("old-1");
    assert!(
        before.is_err(),
        "a stored session has no live agent until resumed"
    );

    studio.resume_session("old-1").unwrap();

    // The seeded history is visible through the normal transcript path, so the
    // UI needs no special case for a resumed session.
    let after = studio.transcript("old-1").unwrap();
    assert_eq!(after.len(), 1, "the stored user message is restored: {after:?}");
    assert_eq!(after[0].text, "first question");

    // And it is live: sending appends to the restored log.
    studio
        .send_message("old-1", "second question".into(), "m2".into())
        .await
        .unwrap();
    let grown = studio.transcript("old-1").unwrap();
    assert!(
        grown.len() > 1,
        "the new turn was appended to the restored history: {grown:?}"
    );
    assert_eq!(grown[0].text, "first question", "and the old turn survived");
}

#[tokio::test]
async fn resuming_then_restarting_does_not_duplicate_the_history() {
    // The dangerous failure mode of seeding from a file: if the seed events are
    // re-broadcast to the persistence backend, the next flush *appends the whole
    // history again*, and the file grows a second copy of every message on each
    // resume. The bug is invisible within one process (the in-memory session is
    // correct) and only shows up after a restart reads the file back.
    //
    // So this asserts on the *reloaded* history, across two studios.
    let dir = tmpdir("resume-no-dup");
    seeded_session(&dir, "old-1", "first question");

    {
        let studio = Studio::with_hook(None, None, dir.clone()).await.unwrap();
        studio.resume_session("old-1").unwrap();
        studio
            .send_message("old-1", "second question".into(), "m2".into())
            .await
            .unwrap();
    }

    // A genuinely fresh process: only the file survives.
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let rows = studio.list_sessions();
    let row = rows.iter().find(|r| r.id == "old-1").expect("still listed");

    let users = {
        let mut events = studio.stored_events("old-1").expect("stored log readable");
        dsh_rs::session::repair_crash_turns(&mut events);
        events
            .iter()
            .filter(|e| matches!(&e.data, dsh_rs::types::SessionEventData::UserMessage { .. }))
            .count()
    };
    assert_eq!(
        users, 2,
        "the two user messages must each appear once — a rewritten seed would \
         double them on every resume. Row: {row:?}"
    );
}

// ---------------------------------------------------------------------------
// Model rebind: switching a live session's provider/model without losing history
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rebinding_the_model_keeps_the_transcript_and_resumes_the_driver() {
    // The headline of the model-switch feature: an existing session is
    // re-pointed at a new provider/model *in place*. The JSONL must survive (the
    // soft-unbind drops the live driver but never deletes the file), the old
    // history must still be visible, and the agent must be live again so the
    // next turn appends on top rather than starting over.
    let dir = tmpdir("rebind");
    let studio = Studio::with_hook(None, None, dir.clone()).await.unwrap();

    studio
        .create_agent(Some("a1".into()), "mock".into(), "mock-1".into(), Some("/tmp".into()))
        .expect("agent created");
    studio
        .send_message("a1", "before the switch".into(), "u1".into())
        .await
        .expect("first turn completes");

    let before = studio.transcript("a1").unwrap();
    assert!(before.iter().any(|m| m.text == "before the switch"));

    // Rebind to a different mock model. `mock` is always registered, so this
    // exercises the rebind path itself rather than provider validation.
    let id = studio
        .rebind_agent_model("a1", "mock".into(), "mock-2".into())
        .await
        .expect("rebind succeeds");
    assert_eq!(id, "a1", "the same session id is reused");

    // The old history survived the switch…
    let after = studio.transcript("a1").unwrap();
    assert!(
        after.iter().any(|m| m.text == "before the switch"),
        "rebinding must not erase the transcript: {after:?}"
    );

    // …and the agent is live again, so the next turn appends to it.
    studio
        .send_message("a1", "after the switch".into(), "u2".into())
        .await
        .expect("second turn completes");
    let grown = studio.transcript("a1").unwrap();
    assert!(
        grown.iter().any(|m| m.text == "after the switch"),
        "the rebound agent must accept new turns: {grown:?}"
    );
    assert!(
        grown.len() > after.len(),
        "the new turn was appended, not a fresh log: {grown:?}"
    );

    // The transcript file is still on disk — soft-unbind must not delete it.
    assert!(
        dir.join("sessions").join("a1.jsonl").exists(),
        "the JSONL must outlive the rebind"
    );
}

#[tokio::test]
async fn rebinding_to_an_unregistered_provider_is_a_clear_error() {
    // The UI's provider picker can offer a provider that is not actually wired
    // up (bad base_url/api_key). Rebinding must refuse with an actionable
    // message rather than silently pretending to switch.
    let dir = tmpdir("rebind-bad-provider");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .create_agent(Some("a1".into()), "mock".into(), "mock-1".into(), Some("/tmp".into()))
        .expect("agent created");

    let err = studio
        .rebind_agent_model("a1", "ghost-provider".into(), "some-model".into())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("ghost-provider") && err.contains("not registered"),
        "the error must name the provider and say it is unregistered, got: {err}"
    );

    // And the original agent is untouched — a failed rebind is a no-op.
    studio
        .send_message("a1", "still works".into(), "u1".into())
        .await
        .expect("the agent still runs on the old model");
}

#[tokio::test]
async fn rebinding_a_session_that_was_never_live_reads_it_from_disk() {
    // A stored-but-not-resumed session is a legitimate rebind target: the file
    // is the source of truth, and the rebound agent comes up live with that
    // history already loaded.
    let dir = tmpdir("rebind-stored");
    seeded_session(&dir, "old-1", "seeded question");

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    // Not resumed: no live agent yet.
    assert!(studio.transcript("old-1").is_err());

    studio
        .rebind_agent_model("old-1", "mock".into(), "mock-2".into())
        .await
        .expect("rebinding a stored session succeeds");

    // Now it is live and carries the seeded history.
    let t = studio.transcript("old-1").unwrap();
    assert_eq!(t.len(), 1, "the seeded user message is restored: {t:?}");
    assert_eq!(t[0].text, "seeded question");

    studio
        .send_message("old-1", "follow up".into(), "u2".into())
        .await
        .expect("the rebound stored session is live");
    assert!(studio.transcript("old-1").unwrap().iter().any(|m| m.text == "follow up"));
}

#[tokio::test]
async fn rebinding_a_missing_session_is_an_error() {
    let dir = tmpdir("rebind-missing");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let err = studio
        .rebind_agent_model("nope", "mock".into(), "mock-2".into())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not found"), "expected a not-found error, got: {err}");
}

#[tokio::test]
async fn list_models_reflects_the_configured_model_lists() {
    // The openhanako ModelSelector reads this command. When the config carries
    // per-provider model lists, every entry must come back tagged with its
    // provider so the UI can group them.
    let dir = tmpdir("list-models");
    std::fs::write(
        dir.join("studio.json"),
        json!({
            "extra": {
                "llm": {
                    "model_lists": {
                        "deepseek": ["deepseek-chat", "deepseek-reasoner"],
                        "openai": [{"id": "gpt-4o"}]
                    },
                    "current": { "provider": "deepseek", "model": "deepseek-chat" }
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let models = studio.list_models();

    let ids: Vec<&str> = models.iter().filter_map(|m| m["id"].as_str()).collect();
    assert!(ids.contains(&"deepseek-chat"), "string entries listed: {models:?}");
    assert!(ids.contains(&"deepseek-reasoner"), "every entry listed: {models:?}");
    assert!(ids.contains(&"gpt-4o"), "object entries (`id`) listed: {models:?}");

    // Each entry carries its provider — the picker groups by it.
    let chat = models.iter().find(|m| m["id"] == "deepseek-chat").unwrap();
    assert_eq!(chat["provider"], "deepseek");
    let gpt = models.iter().find(|m| m["id"] == "gpt-4o").unwrap();
    assert_eq!(gpt["provider"], "openai");
}

#[tokio::test]
async fn list_models_falls_back_to_the_current_selection() {
    // With no configured lists, the picker must still show *something* — the
    // current provider/model — rather than an empty selector.
    let dir = tmpdir("list-models-fallback");
    std::fs::write(
        dir.join("studio.json"),
        json!({
            "extra": {
                "llm": { "current": { "provider": "deepseek", "model": "deepseek-chat" } }
            }
        })
        .to_string(),
    )
    .unwrap();

    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    let models = studio.list_models();
    assert_eq!(models.len(), 1, "the fallback is the current selection: {models:?}");
    assert_eq!(models[0]["id"], "deepseek-chat");
    assert_eq!(models[0]["provider"], "deepseek");
}
