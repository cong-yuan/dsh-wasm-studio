// The frontend and backend must agree on the command surface.
//
// Run with: node --test "src/**/*.test.ts"
//
// `src/lib/api.ts` is the only place the frontend names a Tauri command, and
// `src-tauri/src/lib.rs` is the only place the backend registers one. Nothing
// links them at build time, so a command can exist on one side and not the
// other — and it fails at runtime, in the window, as "command not found".
//
// That is not hypothetical here: the session list sat in this repo calling
// `list_agents` while the backend's session capability was already complete,
// and the mismatch went unnoticed because nothing ever compared the two lists.
// This is that comparison, done at test time instead of at click time.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..");

/** Command names the frontend invokes: `invoke<T>("name", ...)`. */
function frontendCommands(): Set<string> {
  const src = readFileSync(join(ROOT, "src/lib/api.ts"), "utf8");
  const out = new Set<string>();
  // Covers `invoke("x"`, `invoke<T>("x"`, and `invoke<A, B>("x"`.
  for (const m of src.matchAll(/invoke(?:<[^>]*>)?\(\s*"([a-z_]+)"/g)) {
    out.add(m[1]);
  }
  return out;
}

/** Command names the backend registers in `generate_handler![...]`. */
function backendCommands(): Set<string> {
  const src = readFileSync(join(ROOT, "src-tauri/src/lib.rs"), "utf8");
  const block = src.match(/generate_handler!\[([\s\S]*?)\]/);
  assert.ok(block, "could not find generate_handler! in src-tauri/src/lib.rs");
  const out = new Set<string>();
  for (const m of block[1].matchAll(/commands::([a-z_]+)/g)) {
    out.add(m[1]);
  }
  return out;
}

test("every command the frontend invokes is registered on the backend", () => {
  const fe = frontendCommands();
  const be = backendCommands();
  const missing = [...fe].filter((c) => !be.has(c)).sort();
  assert.deepEqual(
    missing,
    [],
    `the frontend calls commands the backend does not register: ${missing.join(", ")}`,
  );
});

test("the two surfaces are actually being read (guards a silently-empty scan)", () => {
  // Without this, a regex that stops matching would make the test above pass
  // vacuously — the worst possible outcome for a consistency check.
  assert.ok(frontendCommands().size > 10, "frontend scan found too few commands");
  assert.ok(backendCommands().size > 10, "backend scan found too few commands");
});

test("resuming and listing sessions are wired on both sides", () => {
  // Named explicitly because they are the feature this suite exists to protect:
  // a session on disk must be listable and continuable from the UI.
  const fe = frontendCommands();
  const be = backendCommands();
  for (const cmd of ["list_sessions", "resume_session"]) {
    assert.ok(fe.has(cmd), `frontend never invokes ${cmd}`);
    assert.ok(be.has(cmd), `backend does not register ${cmd}`);
  }
});
