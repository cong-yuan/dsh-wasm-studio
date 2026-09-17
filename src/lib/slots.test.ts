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
  globMatch,
  isBuiltinRoute,
  normalizePath,
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

// --- Adjustments: a later plugin reshaping earlier UI ----------------------
//
// This is the capability the project exists for, so the tests are deliberately
// blunt about the two properties that make it usable: a later plugin wins, and
// releasing the adjuster restores the original.

test("a later plugin can hide an earlier plugin's contribution", () => {
  const reg = new SlotRegistry();
  reg.openSlot("app", "settings.tabs");
  reg.claim("llm-ui", "settings.tabs", 0, "LlmPanel");
  assert.equal(reg.mounts().length, 1, "visible before adjustment");

  reg.setAdjustments("curator", [
    { owner: "curator", slot: "settings.tabs", from: "llm-ui", action: "hide" },
  ]);

  assert.equal(reg.mounts().length, 0, "hidden after adjustment");
  // The claim still exists — hiding is not deletion.
  assert.equal(reg.resolve().length, 1);
  assert.equal(reg.resolve()[0].hidden, true);
});

test("releasing the adjuster restores the hidden contribution (reversibility)", () => {
  const reg = new SlotRegistry();
  reg.openSlot("app", "settings.tabs");
  reg.claim("llm-ui", "settings.tabs", 0, "LlmPanel");
  reg.setAdjustments("curator", [
    { owner: "curator", slot: "*", action: "hide" },
  ]);
  assert.equal(reg.mounts().length, 0);

  reg.release("curator");
  assert.equal(reg.mounts().length, 1, "back to declared state");
  assert.equal(reg.listAdjustments().length, 0, "adjustment is gone too");
});

test("a later plugin can re-show what an earlier plugin hid (load order wins)", () => {
  const reg = new SlotRegistry();
  reg.openSlot("app", "settings.tabs");
  reg.claim("llm-ui", "settings.tabs", 0, "LlmPanel");
  reg.setAdjustments("hider", [{ owner: "hider", slot: "*", action: "hide" }]);
  assert.equal(reg.mounts().length, 0);

  reg.setAdjustments("restorer", [
    { owner: "restorer", slot: "settings.tabs", from: "llm-ui", action: "unhide" },
  ]);
  assert.equal(reg.mounts().length, 1, "later unhide wins");
});

test("priority adjustments reorder a slot, absolutely and by delta", () => {
  const reg = new SlotRegistry();
  reg.openSlot("app", "settings.tabs");
  reg.claim("a", "settings.tabs", 0, "A");
  reg.claim("b", "settings.tabs", 10, "B");
  assert.deepEqual(reg.mountsFor("settings.tabs").map((c) => c.owner), ["a", "b"]);

  reg.setAdjustments("curator", [
    { owner: "curator", slot: "settings.tabs", from: "b", action: "priority", to: -5 },
  ]);
  assert.deepEqual(
    reg.mountsFor("settings.tabs").map((c) => c.owner),
    ["b", "a"],
    "b moved to the front",
  );

  // A delta applies to the contribution's own priority.
  reg.setAdjustments("nudger", [
    { owner: "nudger", slot: "settings.tabs", from: "a", action: "priority", by: -100 },
  ]);
  assert.deepEqual(reg.mountsFor("settings.tabs").map((c) => c.owner), ["a", "b"]);
});

test("replace redirects rendering to the adjusting plugin's component", () => {
  const reg = new SlotRegistry();
  reg.openSlot("app", "settings.tabs");
  reg.claim("llm-ui", "settings.tabs", 0, "Original");

  reg.setAdjustments("curator", [
    {
      owner: "curator",
      slot: "settings.tabs",
      from: "llm-ui",
      action: "replace",
      component: "Better",
    },
  ]);

  const m = reg.mountsFor("settings.tabs")[0];
  assert.equal(m.component, "Better", "renders the substitute");
  assert.equal(m.renderOwner, "curator", "factory comes from the adjuster");
  assert.equal(m.owner, "llm-ui", "claim stays attributed to the original");
});

test("a replace with no component is a no-op, not a breakage", () => {
  const reg = new SlotRegistry();
  reg.openSlot("app", "settings.tabs");
  reg.claim("llm-ui", "settings.tabs", 0, "Original");
  reg.setAdjustments("curator", [
    { owner: "curator", slot: "*", action: "replace" },
  ]);
  const m = reg.mountsFor("settings.tabs")[0];
  assert.equal(m.component, "Original");
  assert.equal(m.renderOwner, undefined);
});

test("globs target by slot and by owner, including across all slots", () => {
  const reg = new SlotRegistry();
  reg.openSlot("app", "settings.tabs");
  reg.openSlot("app", "dashboard.cards");
  reg.claim("noisy", "settings.tabs", 0, "A");
  reg.claim("noisy", "dashboard.cards", 0, "B");
  reg.claim("quiet", "settings.tabs", 0, "C");

  // Everywhere `noisy` contributes.
  reg.setAdjustments("curator", [
    { owner: "curator", slot: "*", from: "noisy", action: "hide" },
  ]);
  assert.deepEqual(
    reg.mounts().map((m) => m.contribution.owner),
    ["quiet"],
    "only noisy's two contributions were hidden",
  );

  // Re-target by slot instead. `settings.*` is a slot glob, so it hides the
  // two settings.tabs contributions and leaves dashboard.cards alone.
  reg.setAdjustments("curator", [
    { owner: "curator", slot: "settings.*", action: "hide" },
  ]);
  assert.deepEqual(
    reg.mounts().map((m) => m.contribution.owner),
    ["noisy"],
    "dashboard.cards survives a settings.* adjustment",
  );
  assert.deepEqual(reg.mounts().map((m) => m.slot), ["dashboard.cards"]);
});

test("adjustments are idempotent across re-syncs", () => {
  const reg = new SlotRegistry();
  reg.openSlot("app", "settings.tabs");
  reg.claim("llm-ui", "settings.tabs", 0, "LlmPanel");
  const adj = [{ owner: "curator", slot: "*", action: "hide" as const }];

  reg.setAdjustments("curator", adj);
  reg.setAdjustments("curator", adj);
  reg.setAdjustments("curator", adj);

  assert.equal(reg.listAdjustments().length, 1, "not duplicated");
  assert.equal(reg.mounts().length, 0);
});

test("an adjustment to a pending slot applies once that slot appears", () => {
  const reg = new SlotRegistry();
  // `llm-ui` opens the slot; `theme` claims into it before it exists.
  reg.claim("theme", "llm-ui.config", 0, "ThemePanel");
  reg.setAdjustments("curator", [
    { owner: "curator", slot: "llm-ui.*", action: "hide" },
  ]);
  assert.equal(reg.mounts().length, 0, "pending claim renders nothing anyway");

  reg.openSlot("llm-ui", "llm-ui.config");
  assert.equal(reg.mounts().length, 0, "still hidden once the slot opens");
  assert.equal(reg.resolve().length, 1);
  assert.equal(reg.resolve()[0].hidden, true, "the adjustment reached it");
});

test("globMatch handles literals, prefixes and full wildcards", () => {
  assert.equal(globMatch("*", "anything.at.all"), true);
  assert.equal(globMatch(undefined, "x"), true, "undefined matches all");
  assert.equal(globMatch("a.b", "a.b"), true);
  assert.equal(globMatch("a.b", "a.bc"), false, "exact, not prefix");
  assert.equal(globMatch("a.*", "a.b"), true);
  assert.equal(globMatch("a.*", "ab"), false);
  assert.equal(globMatch("*.tabs", "settings.tabs"), true);
  assert.equal(globMatch("a.c", "a.b"), false, "dots are literal, not regex");
});

// --- Conflict reporting ----------------------------------------------------
//
// Resolution is by load order (deliberate: a later plugin may override an
// earlier one). But two plugins disagreeing about the same contribution used to
// be fully silent — a panel vanishes and nothing says why. These pin the
// reporting, NOT the resolution order.

test("reports two plugins hiding the same contribution", () => {
  const reg = new SlotRegistry();
  reg.openSlot("app", "settings.tabs");
  reg.claim("llm-ui", "settings.tabs", 0, "Panel");
  reg.setAdjustments("hider-a", [
    { owner: "hider-a", slot: "settings.tabs", from: "llm-ui", action: "hide" },
  ]);
  reg.setAdjustments("hider-b", [
    { owner: "hider-b", slot: "*", action: "hide" },
  ]);

  const conflicts = reg.adjustmentConflicts();
  assert.equal(conflicts.length, 1);
  assert.equal(conflicts[0].action, "hide");
  assert.equal(conflicts[0].winner, "hider-b", "last in load order wins");
  assert.deepEqual(conflicts[0].losers, ["hider-a"]);
  // And the resolution itself is unchanged.
  assert.equal(reg.mounts().length, 0);
});

test("reports two plugins replacing the same contribution", () => {
  const reg = new SlotRegistry();
  reg.openSlot("app", "settings.tabs");
  reg.claim("llm-ui", "settings.tabs", 0, "Panel");
  reg.setAdjustments("a", [
    { owner: "a", slot: "*", action: "replace", component: "FromA" },
  ]);
  reg.setAdjustments("b", [
    { owner: "b", slot: "*", action: "replace", component: "FromB" },
  ]);

  const conflicts = reg.adjustmentConflicts();
  assert.equal(conflicts.length, 1);
  assert.equal(conflicts[0].action, "replace");
  assert.equal(conflicts[0].winner, "b");
  assert.equal(reg.mountsFor("settings.tabs")[0].component, "FromB", "b won");
});

test("hide-then-unhide is an override, not a conflict", () => {
  const reg = new SlotRegistry();
  reg.openSlot("app", "settings.tabs");
  reg.claim("llm-ui", "settings.tabs", 0, "Panel");
  reg.setAdjustments("hider", [{ owner: "hider", slot: "*", action: "hide" }]);
  reg.setAdjustments("restorer", [
    { owner: "restorer", slot: "settings.tabs", from: "llm-ui", action: "unhide" },
  ]);
  // Different actions on the same target: that is the intended override path.
  assert.deepEqual(reg.adjustmentConflicts(), []);
  assert.equal(reg.mounts().length, 1, "the unhide took effect");
});

test("adjustments to different contributions are not conflicts", () => {
  const reg = new SlotRegistry();
  reg.openSlot("app", "settings.tabs");
  reg.claim("a", "settings.tabs", 0, "A");
  reg.claim("b", "settings.tabs", 0, "B");
  reg.setAdjustments("x", [{ owner: "x", slot: "settings.tabs", from: "a", action: "hide" }]);
  reg.setAdjustments("y", [{ owner: "y", slot: "settings.tabs", from: "b", action: "hide" }]);
  assert.deepEqual(reg.adjustmentConflicts(), [], "they target different claims");
});

test("one plugin hiding the same thing twice is not a conflict", () => {
  const reg = new SlotRegistry();
  reg.openSlot("app", "settings.tabs");
  reg.claim("llm-ui", "settings.tabs", 0, "Panel");
  reg.setAdjustments("solo", [
    { owner: "solo", slot: "settings.tabs", action: "hide" },
    { owner: "solo", slot: "*", action: "hide" },
  ]);
  assert.deepEqual(reg.adjustmentConflicts(), [], "a plugin cannot conflict with itself");
});

test("a conflict disappears when the losing plugin is released", () => {
  const reg = new SlotRegistry();
  reg.openSlot("app", "settings.tabs");
  reg.claim("llm-ui", "settings.tabs", 0, "Panel");
  reg.setAdjustments("a", [{ owner: "a", slot: "*", action: "hide" }]);
  reg.setAdjustments("b", [{ owner: "b", slot: "*", action: "hide" }]);
  assert.equal(reg.adjustmentConflicts().length, 1);

  reg.release("b");
  assert.deepEqual(reg.adjustmentConflicts(), [], "no longer contested");
});

// --- Contributed routes ----------------------------------------------------
//
// A contributed page and its nav entry are one record, so they cannot drift.
// These pin the rules a plugin author depends on.

test("a plugin can contribute a route with a nav entry", () => {
  const reg = new SlotRegistry();
  reg.setRoutes("usage", [
    { path: "usage", owner: "usage", component: "UsagePage", title: "Usage", icon: "◷", nav: true },
  ]);
  const r = reg.routeFor("usage");
  assert.ok(r);
  assert.equal(r.component, "UsagePage");
  assert.deepEqual(
    reg.navRoutes().map((n) => n.path),
    ["usage"],
  );
});

test("nav: false contributes a page with no sidebar entry", () => {
  const reg = new SlotRegistry();
  reg.setRoutes("p", [
    { path: "detail", owner: "p", component: "Detail", title: "Detail", nav: false },
  ]);
  assert.ok(reg.routeFor("detail"), "the page resolves");
  assert.deepEqual(reg.navRoutes(), [], "but nothing is added to the sidebar");
});

test("a nav entry without a label is not an entry", () => {
  const reg = new SlotRegistry();
  reg.setRoutes("p", [{ path: "x", owner: "p", component: "C", nav: true }]);
  assert.deepEqual(reg.navRoutes(), [], "nothing to display, so nothing is shown");
});

test("route paths normalize so lookalikes cannot collide", () => {
  const reg = new SlotRegistry();
  reg.setRoutes("p", [{ path: "/usage/", owner: "p", component: "C", nav: false }]);
  for (const variant of ["usage", "/usage", "usage/", "//usage"]) {
    assert.ok(reg.routeFor(variant), `"${variant}" must resolve to the same route`);
  }
});

test("one plugin declaring two lookalike paths is an error, not silent shadowing", () => {
  const reg = new SlotRegistry();
  // `usage` and `/usage/` are the same route; accepting both would mean one of
  // them is unreachable and `routeFor` could only ever return one.
  assert.throws(
    () =>
      reg.setRoutes("p", [
        { path: "usage", owner: "p", component: "A", nav: true },
        { path: "/usage/", owner: "p", component: "B", nav: true },
      ]),
    /route "usage" is contributed by both "p" and "p"/,
  );
});

test("two plugins contributing the same path is an error, not load-order luck", () => {
  const reg = new SlotRegistry();
  reg.setRoutes("a", [{ path: "usage", owner: "a", component: "A", nav: true }]);
  assert.throws(
    () => reg.setRoutes("b", [{ path: "usage", owner: "b", component: "B", nav: true }]),
    /route "usage" is contributed by both "a" and "b"/,
  );
});

test("re-registering the same owner's routes is idempotent and replaces them", () => {
  const reg = new SlotRegistry();
  reg.setRoutes("p", [{ path: "a", owner: "p", component: "C1", nav: true }]);
  reg.setRoutes("p", [
    { path: "a", owner: "p", component: "C2", nav: true },
    { path: "b", owner: "p", component: "C3", nav: true },
  ]);
  assert.equal(reg.routeFor("a")?.component, "C2", "the component was replaced");
  assert.ok(reg.routeFor("b"));
  assert.equal(reg.listRoutes().length, 2, "no accumulation across re-syncs");
});

test("a plugin that stops declaring a route loses it", () => {
  const reg = new SlotRegistry();
  reg.setRoutes("p", [
    { path: "a", owner: "p", component: "C1", nav: true },
    { path: "b", owner: "p", component: "C2", nav: true },
  ]);
  reg.setRoutes("p", [{ path: "a", owner: "p", component: "C1", nav: true }]);
  assert.ok(reg.routeFor("a"));
  assert.equal(reg.routeFor("b"), undefined, "the dropped route is gone");
});

test("unloading a plugin removes its routes and nav entries", () => {
  const reg = new SlotRegistry();
  reg.setRoutes("p", [{ path: "a", owner: "p", component: "C", title: "A", nav: true }]);
  assert.equal(reg.navRoutes().length, 1);
  reg.release("p");
  assert.equal(reg.routeFor("a"), undefined);
  assert.deepEqual(reg.navRoutes(), [], "and the nav entry goes with it");
});

test("built-in pages are recognised, so a plugin cannot silently shadow one", () => {
  assert.equal(isBuiltinRoute("chat"), true);
  assert.equal(isBuiltinRoute("/settings/"), true, "normalized before comparison");
  assert.equal(isBuiltinRoute(""), true, "the overview page");
  assert.equal(isBuiltinRoute("usage"), false, "a plugin path is free");
});

test("normalizePath collapses leading, trailing and doubled slashes", () => {
  assert.equal(normalizePath("usage"), "usage");
  assert.equal(normalizePath("/usage"), "usage");
  assert.equal(normalizePath("usage/"), "usage");
  assert.equal(normalizePath("//usage"), "usage");
  assert.equal(normalizePath("tools//usage/"), "tools/usage");
});
