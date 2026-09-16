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

    // …and unloading removes it.
    studio.unmount_slot("alpha").await.unwrap();
    let cfg = wasm_plugin_host::Config::load(studio.config_path()).unwrap();
    assert!(cfg.plugins.get("alpha").is_none(), "unload must forget it");
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
    assert_eq!(found.len(), 1, "got {found:?}");
    assert_eq!(found[0].slot, "found");
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

    // Give the watcher a moment to install notify, then write the new build.
    std::thread::sleep(std::time::Duration::from_millis(200));
    std::fs::write(&wasm, wasm_tool_named("alpha", "v2_tool")).unwrap();

    // Poll until the change is observed (bounded, so a failure is a timeout).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut reloaded = false;
    while std::time::Instant::now() < deadline {
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
