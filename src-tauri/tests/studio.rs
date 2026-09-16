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
    let studio = Studio::with_hook(None, dir).await.expect("studio boots");

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

    let studio = Studio::with_hook(None, dir.clone()).await.unwrap();
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

    let studio = Studio::with_hook(None, dir).await.unwrap();
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

    let studio = Studio::with_hook(None, dir).await.unwrap();
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
    let studio = Studio::with_hook(None, dir).await.unwrap();
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

    let studio = Studio::with_hook(None, dir.clone()).await.unwrap();
    let path = wasm.display().to_string();
    studio.mount_slot("alpha", &path, json!(null)).await.unwrap();

    // Corrupt the file, then try to reload: the atomic hot-swap must reject it
    // and keep the working plugin running.
    std::fs::write(&wasm, b"not a wasm module").unwrap();
    let err = studio.reload_slot("alpha", &path);
    assert!(err.is_err(), "a broken build must be rejected");

    // Still loaded and callable.
    assert_eq!(studio.host().list_plugins().len(), 1);
    let out = studio.host().call_tool("alpha_tool", &json!({})).unwrap();
    assert_eq!(out["kind"], "success", "the old plugin keeps serving");
}
