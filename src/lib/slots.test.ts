// Tests for the frontend slot registry's convergence rules.
//
// Run with: node --test src/lib/slots.test.ts
//
// The registry is pure TS, so these exercise the three agreed rules directly:
// order-independence, reactive re-appearance, and conflict-as-error.

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  BUILTIN_SLOTS,
  SlotConflictError,
  SlotRegistry,
} from "./slots.ts";

test("the five built-in slots exist from the start", () => {
  const reg = new SlotRegistry();
  const names = reg.listSlots().map((s) => s.name);
  for (const b of BUILTIN_SLOTS) {
    assert.ok(names.includes(b), `missing builtin slot ${b}`);
  }
  assert.equal(reg.listSlots().length, BUILTIN_SLOTS.length);
});

test("a plugin can open a slot and another can contribute to it", () => {
  const reg = new SlotRegistry();
  reg.openSlot("llm-ui", "llm-ui.config");
  reg.claim("theme", "llm-ui.config", 0, "ThemePanel");
  const mounts = reg.mountsFor("llm-ui.config");
  assert.equal(mounts.length, 1);
  assert.equal(mounts[0].owner, "theme");
  assert.equal(mounts[0].component, "ThemePanel");
});

// --- Rule 1: order-independence -------------------------------------------

test("a claim made BEFORE its slot opens stays pending, then mounts", () => {
  const reg = new SlotRegistry();
  // `theme` loads first and claims into a slot nobody has opened yet.
  reg.claim("theme", "llm-ui.config", 0, "ThemePanel");
  assert.equal(reg.mountsFor("llm-ui.config").length, 0, "pending, not mounted");
  assert.equal(reg.pending().length, 1);
  assert.equal(reg.isSatisfied("theme"), false);

  // Later, `llm-ui` opens the slot — the pending claim mounts with no
  // re-registration.
  reg.openSlot("llm-ui", "llm-ui.config");
  assert.equal(reg.mountsFor("llm-ui.config").length, 1, "now mounted");
  assert.equal(reg.pending().length, 0);
  assert.equal(reg.isSatisfied("theme"), true);
});

test("load order does not change the outcome", () => {
  // Same two plugins, opposite load order -> identical mount set.
  const a = new SlotRegistry();
  a.openSlot("llm-ui", "llm-ui.config");
  a.claim("theme", "llm-ui.config", 0, "T");

  const b = new SlotRegistry();
  b.claim("theme", "llm-ui.config", 0, "T");
  b.openSlot("llm-ui", "llm-ui.config");

  assert.deepEqual(a.mounts(), b.mounts());
});

// --- Rule 2: reactive disappearance / re-appearance ------------------------

test("unloading a slot's owner hides contributors but remembers them", () => {
  const reg = new SlotRegistry();
  reg.openSlot("llm-ui", "llm-ui.config");
  reg.claim("theme", "llm-ui.config", 0, "T");
  assert.equal(reg.mountsFor("llm-ui.config").length, 1);

  // `llm-ui` unloads: its slot goes away, but `theme`'s claim must survive.
  reg.release("llm-ui");
  assert.equal(reg.mountsFor("llm-ui.config").length, 0, "hidden");
  assert.equal(reg.pending().length, 1, "remembered, not dropped");
});

test("when the slot comes back, the contributor reappears automatically", () => {
  const reg = new SlotRegistry();
  reg.openSlot("llm-ui", "llm-ui.config");
  reg.claim("theme", "llm-ui.config", 0, "T");
  reg.release("llm-ui");
  assert.equal(reg.mountsFor("llm-ui.config").length, 0);

  // `llm-ui` is reloaded and reopens the same slot.
  reg.openSlot("llm-ui", "llm-ui.config");
  assert.equal(reg.mountsFor("llm-ui.config").length, 1, "auto-remounted");
});

test("releasing the contributor also removes its claim", () => {
  const reg = new SlotRegistry();
  reg.openSlot("llm-ui", "llm-ui.config");
  reg.claim("theme", "llm-ui.config", 0, "T");
  reg.release("theme");
  assert.equal(reg.mountsFor("llm-ui.config").length, 0);
  assert.equal(reg.pending().length, 0, "the claim is gone, not pending");
});

test("releasing an owner removes only its own slot, not others'", () => {
  const reg = new SlotRegistry();
  reg.openSlot("a", "a.panel");
  reg.openSlot("b", "b.panel");
  reg.release("a");
  const names = reg.listSlots().map((s) => s.name);
  assert.ok(!names.includes("a.panel"));
  assert.ok(names.includes("b.panel"), "b's slot must be untouched");
});

// --- Rule 3: conflicts -----------------------------------------------------

test("two plugins cannot open the same slot name", () => {
  const reg = new SlotRegistry();
  reg.openSlot("a", "shared.panel");
  assert.throws(
    () => reg.openSlot("b", "shared.panel"),
    (e: unknown) => e instanceof SlotConflictError,
  );
});

test("reopening your own slot is idempotent, not a conflict", () => {
  const reg = new SlotRegistry();
  reg.openSlot("a", "a.panel");
  reg.openSlot("a", "a.panel"); // must not throw
  assert.equal(reg.listSlots().filter((s) => s.name === "a.panel").length, 1);
});

test("a plugin may not hijack a built-in slot", () => {
  const reg = new SlotRegistry();
  assert.throws(
    () => reg.openSlot("evil", "settings.tabs"),
    (e: unknown) => e instanceof SlotConflictError,
  );
});

// --- Ordering within a slot ------------------------------------------------

test("contributors render by priority, then registration order", () => {
  const reg = new SlotRegistry();
  reg.openSlot("host", "host.panel");
  reg.claim("late", "host.panel", 10, "L");
  reg.claim("early", "host.panel", -5, "E");
  reg.claim("mid", "host.panel", 0, "M");
  const order = reg.mountsFor("host.panel").map((c) => c.owner);
  assert.deepEqual(order, ["early", "mid", "late"]);
});

test("multiple contributors coexist in one slot", () => {
  const reg = new SlotRegistry();
  reg.openSlot("host", "host.panel");
  for (const o of ["x", "y", "z"]) reg.claim(o, "host.panel", 0, o);
  assert.equal(reg.mountsFor("host.panel").length, 3);
});

// --- Chained slots (a plugin contributing to a plugin's slot) --------------

test("a plugin's own slot can host another plugin's contribution, three deep", () => {
  // A opens `a.panel`; B opens `b.panel` AND contributes into `a.panel`;
  // C contributes into `b.panel`. This is the "later plugin uses an earlier
  // plugin's slot" case, plus one more level.
  const reg = new SlotRegistry();
  reg.openSlot("a", "a.panel");
  reg.openSlot("b", "b.panel");
  reg.claim("b", "a.panel", 0, "BPanel");
  reg.claim("c", "b.panel", 0, "CWidget");

  assert.deepEqual(
    reg.mountsFor("a.panel").map((c) => c.owner),
    ["b"],
  );
  assert.deepEqual(
    reg.mountsFor("b.panel").map((c) => c.owner),
    ["c"],
  );
});

test("dependency chain resolves regardless of registration order", () => {
  // Register C -> B -> A (reverse). Everything must still resolve.
  const reg = new SlotRegistry();
  reg.claim("c", "b.panel", 0, "C");
  reg.claim("b", "a.panel", 0, "B");
  reg.openSlot("b", "b.panel");
  // At this point c is satisfied, b is still pending (a.panel missing).
  assert.equal(reg.mountsFor("b.panel").length, 1);
  assert.equal(reg.mountsFor("a.panel").length, 0);
  reg.openSlot("a", "a.panel");
  assert.equal(reg.mountsFor("a.panel").length, 1, "b mounted once a appeared");
});

// --- Integration with the backend's wire format ----------------------------
//
// These feed the *exact* JSON shape the Rust `UiPlugin` struct serialises
// (mirrored in api.ts as `UiPlugin`) into the registry, so the contract between
// the two sides is pinned by a test rather than by hope.

/** Mirrors `commands::UiPlugin` in the Tauri backend. */
interface UiPluginWire {
  slot: string;
  provides_slots: string[];
  injects_slots: { slot: string; priority: number; component?: string }[];
  assets: Record<string, string>;
}

/** Load a backend `UiPlugin` payload into the registry, as the app will. */
function loadWire(reg: SlotRegistry, p: UiPluginWire): void {
  for (const s of p.provides_slots) reg.openSlot(p.slot, s);
  for (const i of p.injects_slots) reg.claim(p.slot, i.slot, i.priority, i.component);
}

test("backend wire format: B mounts into the slot A opens (A first)", () => {
  const reg = new SlotRegistry();
  const llmUi: UiPluginWire = {
    slot: "llm-ui",
    provides_slots: ["llm-ui.config"],
    injects_slots: [],
    assets: { "entry.js": "register({});" },
  };
  const theme: UiPluginWire = {
    slot: "theme",
    provides_slots: [],
    injects_slots: [{ slot: "llm-ui.config", priority: 0, component: "Theme" }],
    assets: {},
  };
  loadWire(reg, llmUi);
  loadWire(reg, theme);
  assert.deepEqual(
    reg.mountsFor("llm-ui.config").map((c) => c.component),
    ["Theme"],
  );
});

test("backend wire format: same result when B loads before A", () => {
  const reg = new SlotRegistry();
  const theme: UiPluginWire = {
    slot: "theme",
    provides_slots: [],
    injects_slots: [{ slot: "llm-ui.config", priority: 0, component: "Theme" }],
    assets: {},
  };
  const llmUi: UiPluginWire = {
    slot: "llm-ui",
    provides_slots: ["llm-ui.config"],
    injects_slots: [],
    assets: {},
  };
  // Theme first: its claim is pending until llm-ui opens the slot.
  loadWire(reg, theme);
  assert.equal(reg.mountsFor("llm-ui.config").length, 0);
  loadWire(reg, llmUi);
  assert.deepEqual(
    reg.mountsFor("llm-ui.config").map((c) => c.component),
    ["Theme"],
  );
});

test("backend wire format: unloading through release() hides and restores", () => {
  const reg = new SlotRegistry();
  loadWire(reg, {
    slot: "llm-ui",
    provides_slots: ["llm-ui.config"],
    injects_slots: [],
    assets: {},
  });
  loadWire(reg, {
    slot: "theme",
    provides_slots: [],
    injects_slots: [{ slot: "llm-ui.config", priority: 0, component: "Theme" }],
    assets: {},
  });
  reg.release("llm-ui");
  assert.equal(reg.mountsFor("llm-ui.config").length, 0, "hidden");
  loadWire(reg, {
    slot: "llm-ui",
    provides_slots: ["llm-ui.config"],
    injects_slots: [],
    assets: {},
  });
  assert.equal(reg.mountsFor("llm-ui.config").length, 1, "restored");
});
