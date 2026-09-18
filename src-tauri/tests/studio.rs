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
    // The `dsh-web-shell` design rests on this: the shell declares its slots, and
    // a plugin written later contributes to them without knowing anything about
    // the shell. If `provides` did not reach the frontend, every such
    // contribution would silently vanish — a feature-shaped hole with no error.
    let dir = tmpdir("shell-slots");
    let shell = r#"{"name":"shell","abi":1,"tools":[],"ui":{
        "assets":{"entry.js":"studio.register('S', () => {});"},
        "provides":[
          {"name":"dsh-web.sidebar.items","description":"the sidebar list"},
          {"name":"dsh-web.details.items","description":"right column body"}
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
        provided.contains(&"dsh-web.sidebar.items".to_string()),
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
        "injects":[{"slot":"dsh-web.sidebar.items","component":"A","priority":10}]}}"#;
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
        vec!["dsh-web.sidebar.items".to_string()],
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
    // The check is on the *shipped manifest*: load the real `dsh-web-shell.wasm`
    // if it has been built, and assert every declared slot is a `dsh-web.` name.
    // (Skipped when the plugin has not been built yet, so a fresh clone still
    // passes — the plugin is a demo, not a dependency.)
    let built = std::path::Path::new(
        "/Users/yuan/wasm-plugin-host/target/wasm32-wasip1/release/dsh_web_shell.wasm",
    );
    if !built.exists() {
        eprintln!("skipping: dsh-web-shell wasm not built");
        return;
    }
    let dir = tmpdir("shell-manifest");
    let studio = Studio::with_hook(None, None, dir).await.unwrap();
    studio
        .mount_slot("shell", &built.display().to_string(), json!(null))
        .await
        .unwrap();

    let decls = studio.host().ui_decls();
    let provided: Vec<String> = decls
        .iter()
        .flat_map(|(_, ui)| ui.provides.iter().map(|p| p.name.clone()))
        .collect();

    assert!(
        provided.len() >= 10,
        "the shell must open a slot per layout region, found {}: {provided:?}",
        provided.len()
    );
    for name in &provided {
        assert!(
            name.starts_with("dsh-web."),
            "slot names stay in the shell's namespace: {name}"
        );
    }
    // The regions a plugin is most likely to want must be present, since the
    // names are the shell's public contract.
    for required in [
        "dsh-web.sidebar.items",
        "dsh-web.sidebar.footer",
        "dsh-web.conversation.input.dock",
        "dsh-web.details.items",
        "dsh-web.shell.overlay",
    ] {
        assert!(
            provided.iter().any(|p| p == required),
            "`{required}` is part of the contract but was not declared"
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
        "/Users/yuan/wasm-plugin-host/target/wasm32-wasip1/release/dsh_web_shell.wasm",
    );
    if !built.exists() {
        eprintln!("skipping: dsh-web-shell wasm not built");
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
    for required in ["lib/tokens.js", "lib/motion.js", "lib/dom.js", "lib/slots.js"] {
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
