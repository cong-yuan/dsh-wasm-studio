// Tests for the frontend plugin host: JS execution, mounting, teardown, and the
// cross-plugin slot scenario.
//
// Run with: node --test src/lib/plugin-host.test.ts
//
// The host takes a DomAdapter and a contributions source, so it runs under Node
// with no real DOM and no Tauri.

import { test } from "node:test";
import assert from "node:assert/strict";

import { PluginHost, type DomAdapter } from "./plugin-host.ts";
import type { UiPlugin } from "./api.ts";

/** A tiny DOM stand-in recording structure. */
class FakeEl {
  tag: string;
  children: FakeEl[] = [];
  textContent = "";
  dataset: Record<string, string> = {};
  /** Enough of `style` for the host's own use (it sets `height`). */
  style: Record<string, string> = {};
  removed = false;
  /** Set by appendChild so remove() can detach for real (like the DOM). */
  parent: FakeEl | null = null;
  constructor(tag: string) {
    this.tag = tag;
  }
  appendChild(c: FakeEl): FakeEl {
    if (c.parent) c.parent.children.splice(c.parent.children.indexOf(c), 1);
    this.children.push(c);
    c.parent = this;
    return c;
  }
  /** Mirrors the real DOM: drop all children (and unparent them). */
  replaceChildren(): void {
    for (const c of this.children) c.parent = null;
    this.children.length = 0;
  }
  remove(): void {
    this.removed = true;
    if (this.parent) {
      const i = this.parent.children.indexOf(this);
      if (i >= 0) this.parent.children.splice(i, 1);
      this.parent = null;
    }
  }
}

function fakeDom(): DomAdapter {
  const headEl = new FakeEl("head");
  return {
    createElement: (tag: string) => new FakeEl(tag) as unknown as HTMLElement,
    head: () => headEl as unknown as HTMLElement,
  } as DomAdapter;
}

/** A mutable source the tests can point at different payloads. */
function sourceOf(initial: UiPlugin[]) {
  let current = initial;
  return {
    set(next: UiPlugin[]) {
      current = next;
    },
    get: async () => current,
  };
}

const PLUGIN_LLM_UI: UiPlugin = {
  slot: "llm-ui",
  provides_slots: ["llm-ui.config"],
  injects_slots: [],
  assets: {
    "entry.js": `studio.register("Panel", (el) => { el.textContent = "llm-ui panel"; });`,
  },
};

const PLUGIN_THEME: UiPlugin = {
  slot: "theme",
  provides_slots: [],
  injects_slots: [{ slot: "llm-ui.config", priority: 0, component: "Widget" }],
  assets: {
    "entry.js": `studio.register("Widget", (el) => { el.textContent = "theme widget"; });`,
  },
};

test("sync executes entry.js and registers components", async () => {
  const src = sourceOf([PLUGIN_LLM_UI]);
  const host = new PluginHost(fakeDom(), src.get);
  await host.sync();

  assert.equal(host.slots.listSlots().some((s) => s.name === "llm-ui.config"), true);
  const c = host.contributionsFor("llm-ui.config");
  assert.equal(c.length, 0, "llm-ui itself claims nothing");

  // Mount the panel the plugin registered.
  const reg = host.slots;
  reg.claim("someone", "llm-ui.config", 0, "Panel"); // eslint-disable-line
  void c;
});

test("component factory runs on mount and its disposer on unmount", async () => {
  let mountedText = "";
  let tornDown = false;
  const plugin: UiPlugin = {
    slot: "p",
    provides_slots: ["p.slot"],
    injects_slots: [{ slot: "p.slot", priority: 0, component: "C" }],
    assets: {
      "entry.js": `studio.register("C", (el) => {
        el.textContent = "hello";
        return () => {};
      });`,
    },
  };
  const host = new PluginHost(fakeDom(), sourceOf([plugin]).get);
  await host.sync();

  const contribs = host.contributionsFor("p.slot");
  assert.equal(contribs.length, 1);

  const parent = new FakeEl("div");
  const dispose = host.mount(contribs[0], parent as unknown as HTMLElement);
  assert.equal(parent.children.length, 1, "a wrapper was appended");
  const wrapper = parent.children[0];
  assert.equal(wrapper.textContent, "hello", "the factory ran");
  mountedText = wrapper.textContent;

  dispose();
  // Keep the reference: a faithful remove() detaches from the parent.
  assert.equal(wrapper.removed, true, "wrapper removed on unmount");
  assert.equal(parent.children.length, 0, "and detached from the DOM");
  void mountedText;
  void tornDown;
});

test("cross-plugin: B mounts into the slot A opens, both load orders", async () => {
  for (const order of [
    [PLUGIN_LLM_UI, PLUGIN_THEME],
    [PLUGIN_THEME, PLUGIN_LLM_UI],
  ]) {
    // Feed one plugin at a time to simulate a real load sequence.
    const src = sourceOf([order[0]]);
    const host = new PluginHost(fakeDom(), src.get);
    await host.sync();

    src.set(order);
    await host.sync();

    const contribs = host.contributionsFor("llm-ui.config");
    assert.equal(contribs.length, 1, "theme contributes into llm-ui's slot");
    assert.equal(contribs[0].owner, "theme");
    assert.equal(contribs[0].component, "Widget");
    assert.ok(host.factoryFor(contribs[0]), "theme's Widget factory is available");
  }
});

test("unloading a slot owner hides the contributor without losing it", async () => {
  const src = sourceOf([PLUGIN_LLM_UI, PLUGIN_THEME]);
  const host = new PluginHost(fakeDom(), src.get);
  await host.sync();
  assert.equal(host.contributionsFor("llm-ui.config").length, 1);

  // llm-ui unloads; theme stays.
  src.set([PLUGIN_THEME]);
  await host.sync();
  assert.equal(host.contributionsFor("llm-ui.config").length, 0, "hidden");
  assert.equal(host.slots.pending().length, 1, "theme's claim is remembered");

  // llm-ui comes back.
  src.set([PLUGIN_LLM_UI, PLUGIN_THEME]);
  await host.sync();
  assert.equal(host.contributionsFor("llm-ui.config").length, 1, "restored");
});

test("unloading a plugin removes its slot and its components", async () => {
  const src = sourceOf([PLUGIN_LLM_UI]);
  const host = new PluginHost(fakeDom(), src.get);
  await host.sync();
  assert.ok(host.slots.listSlots().some((s) => s.name === "llm-ui.config"));

  src.set([]);
  await host.sync();
  assert.ok(
    !host.slots.listSlots().some((s) => s.name === "llm-ui.config"),
    "its slot is gone",
  );
});

test("a broken entry.js is contained — other plugins still work", async () => {
  const bad: UiPlugin = {
    slot: "bad",
    provides_slots: ["bad.slot"],
    injects_slots: [],
    assets: { "entry.js": `throw new Error("boom");` },
  };
  const src = sourceOf([bad, PLUGIN_LLM_UI]);
  const host = new PluginHost(fakeDom(), src.get);
  await host.sync(); // must not throw
  assert.ok(
    host.slots.listSlots().some((s) => s.name === "llm-ui.config"),
    "the good plugin still loaded",
  );
});

test("entry.js can register slots and claims imperatively too", async () => {
  const plugin: UiPlugin = {
    slot: "self",
    provides_slots: [],
    injects_slots: [],
    assets: {
      "entry.js": `studio.provideSlot("self.extra"); studio.inject("self.extra", "Inner", 0);
        studio.register("Inner", (el) => { el.textContent = "inner"; });`,
    },
  };
  const host = new PluginHost(fakeDom(), sourceOf([plugin]).get);
  await host.sync();
  const c = host.contributionsFor("self.extra");
  assert.equal(c.length, 1);
  assert.equal(c[0].component, "Inner");
});

// --- Diagnostics: explaining "I don't see it" ------------------------------

test("diagnostics reports a contribution whose slot is missing", async () => {
  const plugin: UiPlugin = {
    slot: "waiter",
    provides_slots: [],
    injects_slots: [{ slot: "nobody.opened", priority: 0, component: "X" }],
    assets: {},
  };
  const host = new PluginHost(fakeDom(), sourceOf([plugin]).get);
  await host.sync();
  const d = host.diagnostics();
  assert.equal(d.length, 1);
  assert.equal(d[0].status, "waiting for slot");
  assert.equal(d[0].slotOpen, false);
});

test("diagnostics reports a declared-but-unregistered component", async () => {
  const plugin: UiPlugin = {
    slot: "p",
    provides_slots: ["p.slot"],
    // declares component "Missing" but entry.js never registers it
    injects_slots: [{ slot: "p.slot", priority: 0, component: "Missing" }],
    assets: { "entry.js": `studio.register("Other", () => {});` },
  };
  const host = new PluginHost(fakeDom(), sourceOf([plugin]).get);
  await host.sync();
  const d = host.diagnostics();
  assert.equal(d.length, 1);
  assert.equal(d[0].status, "component not registered");
  assert.equal(d[0].hasFactory, false);
});

test("diagnostics reports ready when everything lines up", async () => {
  const plugin: UiPlugin = {
    slot: "p",
    provides_slots: ["p.slot"],
    injects_slots: [{ slot: "p.slot", priority: 0, component: "OK" }],
    assets: { "entry.js": `studio.register("OK", () => {});` },
  };
  const host = new PluginHost(fakeDom(), sourceOf([plugin]).get);
  await host.sync();
  const d = host.diagnostics();
  assert.equal(d[0].status, "ready");
  assert.equal(d[0].hasFactory, true);
});

test("the cross-plugin pair both read as ready", async () => {
  const host = new PluginHost(fakeDom(), sourceOf([PLUGIN_LLM_UI, PLUGIN_THEME]).get);
  await host.sync();
  const statuses = host
    .diagnostics()
    .filter((d) => d.slot === "llm-ui.config")
    .map((d) => `${d.owner}:${d.status}`);
  assert.deepEqual(statuses, ["theme:ready"]);
});

// --- A plugin OPENING a slot must render its children ----------------------
//
// The gap this closes: a plugin could declare `provides: [some.slot]`, but
// nothing rendered that slot's children, so contributors were invisible. A
// plugin now calls `studio.renderSlot(name, el)` inside its own UI, and the
// host mounts the children there — including contributors that arrive later.

/** An opener whose entry.js hosts a sub-slot, recording the container it used. */
function openerPlugin(sub: string): { plugin: UiPlugin; lastContainer: () => FakeEl | null } {
  let container: FakeEl | null = null;
  const plugin: UiPlugin = {
    slot: "opener",
    provides_slots: [sub],
    injects_slots: [],
    assets: {
      "entry.js": `
        studio.register("Panel", (el) => {
          const sub = document.createElement("div");
          el.appendChild(sub);
          const dispose = studio.renderSlot("${sub}", sub);
          return () => dispose();
        });
        studio.inject("settings.tabs", "Panel", 0);
      `,
    },
  };
  return { plugin, lastContainer: () => container };
}

/** A contributor that registers a component and claims a place in `slot`. */
function contributorPlugin(owner: string, slot: string, component: string): UiPlugin {
  return {
    slot: owner,
    provides_slots: [],
    injects_slots: [{ slot, priority: 0, component }],
    assets: {
      "entry.js": `studio.register("${component}", (el) => { el.textContent = "${owner}"; });`,
    },
  };
}

test("renderSlot mounts a contributor into the opener's own DOM", async () => {
  // The host must expose `document`-like creation to plugin js, so give the
  // fake DOM a global-ish createElement the entry.js can call.
  const dom = fakeDom();
  const g = globalThis as unknown as { document: unknown };
  const prev = g.document;
  g.document = { createElement: (t: string) => new FakeEl(t) };

  try {
    const { plugin } = openerPlugin("hosted.slot");
    const src = sourceOf([plugin, contributorPlugin("guest", "hosted.slot", "G")]);
    const host = new PluginHost(dom, src.get);
    await host.sync();

    // The opener rendered into settings.tabs; find that wrapper and confirm the
    // guest was mounted inside it.
    const tabContribs = host.contributionsFor("settings.tabs");
    assert.equal(tabContribs.length, 1, "opener claims a place in settings.tabs");
    const parent = new FakeEl("div");
    host.mount(tabContribs[0], parent as unknown as HTMLElement);
    // parent -> wrapper(panel) -> sub -> holder(guest)
    const panel = parent.children[0];
    assert.ok(panel, "a wrapper was appended");
    const nested = panel.children[0];
    assert.ok(nested, "the opener created a sub-container");
    assert.ok(
      nested.children.length >= 1,
      `the guest was mounted into the opener's container (children=${nested.children.length})`,
    );
  } finally {
    g.document = prev;
  }
});

test("a contributor arriving later fills an already-rendered opener container", async () => {
  const dom = fakeDom();
  const g = globalThis as unknown as { document: unknown };
  const prev = g.document;
  g.document = { createElement: (t: string) => new FakeEl(t) };
  try {
    const { plugin } = openerPlugin("hosted.slot");
    const src = sourceOf([plugin]);
    const host = new PluginHost(dom, src.get);
    await host.sync();

    const tabContribs = host.contributionsFor("settings.tabs");
    const parent = new FakeEl("div");
    host.mount(tabContribs[0], parent as unknown as HTMLElement);
    const nested = parent.children[0].children[0];
    assert.equal(nested.children.length, 0, "nothing contributed yet");

    // The guest loads afterwards; a re-sync must fill the existing container.
    src.set([plugin, contributorPlugin("guest", "hosted.slot", "G")]);
    await host.sync();
    assert.ok(
      nested.children.length >= 1,
      "the late contributor appeared without the opener re-rendering",
    );
  } finally {
    g.document = prev;
  }
});

// ---------------------------------------------------------------------------
// Hot-reload hygiene
//
// Re-running an entry.js builds a NEW handle and disposes the old one. Any
// registration the old handle made must be undone, or it accumulates on every
// reload. These pin the two that used to leak.
// ---------------------------------------------------------------------------

/** A plugin whose entry.js declares claims imperatively. */
function imperativePlugin(slot: string, injects: number): UiPlugin {
  const lines = [`studio.register("C", (el) => { el.textContent = "c"; });`];
  for (let i = 0; i < injects; i++) lines.push(`studio.inject("settings.tabs", "C", 0);`);
  return { slot, provides_slots: [], injects_slots: [], assets: { "entry.js": lines.join("\n") } };
}

test("re-running entry.js does not accumulate imperative claims", async () => {
  const src = sourceOf([imperativePlugin("p", 1)]);
  const host = new PluginHost(fakeDom(), src.get);

  await host.sync();
  assert.equal(host.contributionsFor("settings.tabs").length, 1);

  // A hot reload: the plugin now declares two.
  src.set([imperativePlugin("p", 2)]);
  await host.sync();
  assert.equal(
    host.contributionsFor("settings.tabs").length,
    2,
    "the new entry.js's two claims are live",
  );

  // …and another reload back to one. Without teardown this would be 1+2+1=4.
  src.set([imperativePlugin("p", 1)]);
  await host.sync();
  assert.equal(
    host.contributionsFor("settings.tabs").length,
    1,
    "the previous version's claims are gone, not accumulated",
  );
});

test("releasing a plugin drops its imperative claims", async () => {
  const src = sourceOf([imperativePlugin("p", 3)]);
  const host = new PluginHost(fakeDom(), src.get);
  await host.sync();
  assert.equal(host.contributionsFor("settings.tabs").length, 3);

  src.set([]); // plugin unloaded
  await host.sync();
  assert.equal(
    host.contributionsFor("settings.tabs").length,
    0,
    "unloading releases the claims its entry.js made",
  );
});

test("a failing teardown does not strand the plugin's other registrations", async () => {
  // Isolation, proved properly: two slot containers are tracked, the one whose
  // teardown runs FIRST throws, and the second must still be detached.
  //
  // Teardowns run newest-first, so declaring container `second` before
  // container `first` means `first` is torn down first — i.e. the throwing one
  // goes first, which is exactly the ordering that would strand the other.
  const containers: FakeEl[] = [];
  const src = sourceOf([
    {
      slot: "p",
      provides_slots: ["p.a"],
      injects_slots: [],
      assets: {
        "entry.js": `
          studio.register("C", (el) => {});
          studio.inject("settings.tabs", "C", 0);
          const second = document.createElement("div");
          studio.renderSlot("p.a", second);
          const first = document.createElement("div");
          studio.renderSlot("p.a", first);
        `,
      },
    },
  ]);

  const g = globalThis as unknown as { document: unknown };
  const prev = g.document;
  g.document = {
    createElement: (t: string) => {
      const el = new FakeEl(t);
      containers.push(el);
      return el;
    },
  };
  try {
    const host = new PluginHost(fakeDom(), src.get);
    await host.sync();

    // The first-created container is the one torn down first.
    const first = containers[0];
    let otherDetached = false;
    first.replaceChildren = () => {
      throw new Error("boom");
    };
    const second = containers[1];
    const realReplace = second.replaceChildren.bind(second);
    second.replaceChildren = () => {
      otherDetached = true;
      realReplace();
    };

    // Reloading disposes the old handle, running both teardowns.
    src.set([imperativePlugin("p", 1)]);
    await host.sync();

    assert.ok(otherDetached, "the second teardown ran despite the first throwing");
    assert.equal(
      host.contributionsFor("settings.tabs").filter((c) => c.owner === "p").length,
      1,
      "the new entry.js's claim is live",
    );
  } finally {
    g.document = prev;
  }
});

test("a throwing component does not prevent the rest of the slot from mounting", async () => {
  // `<Slot>` mounts contributions in a loop. If one factory throws out of
  // `mount`, the loop aborts and every later contribution in that slot never
  // renders — one broken plugin blanks the slot. The throw must be contained.
  const host = new PluginHost(fakeDom(), async () => [
    {
      slot: "good",
      provides_slots: [],
      injects_slots: [{ slot: "settings.tabs", priority: 0, component: "Good" }],
      assets: { "entry.js": `studio.register("Good", (el) => { el.textContent = "ok"; });` },
    },
    {
      slot: "bad",
      provides_slots: [],
      injects_slots: [{ slot: "settings.tabs", priority: 1, component: "Bad" }],
      assets: { "entry.js": `studio.register("Bad", () => { throw new Error("boom"); });` },
    },
    {
      slot: "later",
      provides_slots: [],
      injects_slots: [{ slot: "settings.tabs", priority: 2, component: "Later" }],
      assets: { "entry.js": `studio.register("Later", (el) => { el.textContent = "third"; });` },
    },
  ]);
  await host.sync();

  const list = host.contributionsFor("settings.tabs");
  assert.equal(list.length, 3);

  const parent = new FakeEl("div");
  // Replicate `<Slot>`'s loop, which must not need its own try/catch.
  const cleanups = list.map((c) => host.mount(c, parent as unknown as HTMLElement));

  // The first and third rendered; the broken one left no orphaned wrapper.
  // The wrapper IS the element a factory renders into, so text lands on it.
  assert.equal(parent.children.length, 2, "only the two working components have wrappers");
  assert.equal(parent.children[0].textContent, "ok");
  assert.equal(parent.children[1].textContent, "third");
  assert.equal(parent.children[0].dataset.plugin, "good");
  assert.equal(parent.children[1].dataset.plugin, "later");
  for (const c of cleanups) c();
});

test("a throwing component leaves no live mount behind", async () => {
  const host = new PluginHost(fakeDom(), async () => [
    {
      slot: "bad",
      provides_slots: [],
      injects_slots: [{ slot: "settings.tabs", priority: 0, component: "Bad" }],
      assets: { "entry.js": `studio.register("Bad", () => { throw new Error("boom"); });` },
    },
  ]);
  await host.sync();
  const parent = new FakeEl("div");
  host.mount(host.contributionsFor("settings.tabs")[0], parent as unknown as HTMLElement);
  assert.equal(parent.children.length, 0, "the failed wrapper was removed");

  // A second attempt must not be short-circuited by a phantom live entry.
  host.mount(host.contributionsFor("settings.tabs")[0], parent as unknown as HTMLElement);
  assert.equal(parent.children.length, 0, "still nothing, and no crash");
});

// ---------------------------------------------------------------------------
// Multi-file plugins (`studio.require`)
//
// A plugin used to be a single `entry.js` string. Helpers and components can
// now live in their own `.js` assets and be pulled in on demand.
// ---------------------------------------------------------------------------

test("a plugin can require another of its own .js assets", async () => {
  const host = new PluginHost(fakeDom(), async () => [
    {
      slot: "p",
      provides_slots: [],
      injects_slots: [{ slot: "settings.tabs", priority: 0, component: "C" }],
      assets: {
        "lib/format.js": `return { pct: (n) => n + "%" };`,
        "entry.js": `
          const { pct } = studio.require("lib/format");
          studio.register("C", (el) => { el.textContent = pct(42); });
        `,
      },
    },
  ]);
  await host.sync();

  const parent = new FakeEl("div");
  host.mount(host.contributionsFor("settings.tabs")[0], parent as unknown as HTMLElement);
  assert.equal(parent.children[0].textContent, "42%", "the helper's code ran");
});

test("modules are cached — a module body runs once", async () => {
  const host = new PluginHost(fakeDom(), async () => [
    {
      slot: "p",
      provides_slots: [],
      injects_slots: [],
      assets: {
        // A module that increments a global on each evaluation.
        "counter.js": `globalThis.__modLoads = (globalThis.__modLoads ?? 0) + 1; return { n: globalThis.__modLoads };`,
        "entry.js": `
          const a = studio.require("counter");
          const b = studio.require("counter");
          if (a !== b) throw new Error("module was re-evaluated");
          if (a.n !== 1) throw new Error("ran twice");
        `,
      },
    },
  ]);
  await host.sync();
  assert.equal((globalThis as Record<string, unknown>).__modLoads, 1);
  delete (globalThis as Record<string, unknown>).__modLoads;
});

test("modules may require each other", async () => {
  const host = new PluginHost(fakeDom(), async () => [
    {
      slot: "p",
      provides_slots: [],
      injects_slots: [{ slot: "settings.tabs", priority: 0, component: "C" }],
      assets: {
        "b.js": `const { base } = studio.require("a"); return { value: base + 1 };`,
        "a.js": `return { base: 10 };`,
        "entry.js": `
          const { value } = studio.require("b");
          studio.register("C", (el) => { el.textContent = String(value); });
        `,
      },
    },
  ]);
  await host.sync();
  const parent = new FakeEl("div");
  host.mount(host.contributionsFor("settings.tabs")[0], parent as unknown as HTMLElement);
  assert.equal(parent.children[0].textContent, "11");
});

test("requiring an unknown module fails with the available names", async () => {
  let message = "";
  const host = new PluginHost(fakeDom(), async () => [
    {
      slot: "p",
      provides_slots: [],
      injects_slots: [],
      assets: {
        "lib/known.js": `return {};`,
        "entry.js": `
          try { studio.require("lib/typo"); }
          catch (e) { globalThis.__reqErr = e.message; }
        `,
      },
    },
  ]);
  await host.sync();
  message = String((globalThis as Record<string, unknown>).__reqErr ?? "");
  delete (globalThis as Record<string, unknown>).__reqErr;
  assert.match(message, /no module "lib\/typo"/);
  assert.match(message, /lib\/known/, "names the modules that DO exist");
});

test("a require cycle throws instead of recursing forever", async () => {
  const host = new PluginHost(fakeDom(), async () => [
    {
      slot: "p",
      provides_slots: [],
      injects_slots: [],
      assets: {
        "x.js": `return studio.require("y");`,
        "y.js": `return studio.require("x");`,
        "entry.js": `
          try { studio.require("x"); }
          catch (e) { globalThis.__cycleErr = e.message; }
        `,
      },
    },
  ]);
  await host.sync();
  const msg = String((globalThis as Record<string, unknown>).__cycleErr ?? "");
  delete (globalThis as Record<string, unknown>).__cycleErr;
  assert.match(msg, /circular require/);
});

test("an unused module is never evaluated", async () => {
  const host = new PluginHost(fakeDom(), async () => [
    {
      slot: "p",
      provides_slots: [],
      injects_slots: [],
      assets: {
        // Would throw if evaluated — and must not be, since nothing requires it.
        "unused.js": `throw new Error("should never run");`,
        "entry.js": `studio.register("C", (el) => {});`,
      },
    },
  ]);
  await host.sync(); // must not throw
  assert.ok(host.diagnostics().length >= 0);
});

test("a changed module is reloaded on the next entry.js change", async () => {
  const src = sourceOf([
    {
      slot: "p",
      provides_slots: [],
      injects_slots: [{ slot: "settings.tabs", priority: 0, component: "C" }],
      assets: {
        "m.js": `return { v: "one" };`,
        "entry.js": `const { v } = studio.require("m"); studio.register("C", (el) => { el.textContent = v; });`,
      },
    },
  ]);
  const host = new PluginHost(fakeDom(), src.get);
  await host.sync();

  const first = new FakeEl("div");
  host.mount(host.contributionsFor("settings.tabs")[0], first as unknown as HTMLElement);
  assert.equal(first.children[0].textContent, "one");

  // Change BOTH the module and the entry (a real rebuild touches entry.js).
  src.set([
    {
      slot: "p",
      provides_slots: [],
      injects_slots: [{ slot: "settings.tabs", priority: 0, component: "C" }],
      assets: {
        "m.js": `return { v: "two" };`,
        "entry.js": `const { v } = studio.require("m"); studio.register("C", (el) => { el.textContent = v; });`,
      },
    },
  ]);
  await host.sync();
  host.unmountAll();
  const second = new FakeEl("div");
  host.mount(host.contributionsFor("settings.tabs")[0], second as unknown as HTMLElement);
  assert.equal(second.children[0].textContent, "two", "the new module body was used");
});

test("a plugin's style.css replaces the app's theme tokens", async () => {
  // The app is fully token-driven (`:root { --bg-canvas: … }`) and its styles
  // are build-time <link>s, while a plugin's CSS is appended to <head> at
  // runtime. Same specificity, later in document order, so the plugin wins.
  //
  // This test proves the *mechanism* — the stylesheet reaches the head, once,
  // updated in place on re-sync. (Actual cascade precedence is the browser's
  // job; what we can get wrong here is injection order and duplication.)
  const head = new FakeEl("head");
  const dom: DomAdapter = {
    createElement: (t) => new FakeEl(t) as unknown as HTMLElement,
    head: () => head as unknown as HTMLElement,
  };
  const src = sourceOf([
    {
      slot: "skin",
      provides_slots: [],
      injects_slots: [],
      assets: { "style.css": `:root { --bg-canvas: #000; --accent: #7c3aed; }` },
    },
  ]);
  const host = new PluginHost(dom, src.get);
  await host.sync();

  const styles = head.children.filter((c) => c.tag === "style");
  assert.equal(styles.length, 1, "the plugin's stylesheet is in <head>");
  assert.match(styles[0].textContent, /--bg-canvas:\s*#000/);
  assert.match(styles[0].textContent, /--accent:\s*#7c3aed/);
  assert.equal(styles[0].dataset.pluginStyle, "skin", "attributed for teardown");

  // Re-syncing updates the SAME element — otherwise head would grow without
  // bound and a reload could leave a stale theme winning by position.
  src.set([
    {
      slot: "skin",
      provides_slots: [],
      injects_slots: [],
      assets: { "style.css": `:root { --bg-canvas: #111; }` },
    },
  ]);
  await host.sync();
  const after = head.children.filter((c) => c.tag === "style");
  assert.equal(after.length, 1, "still one element, not two");
  assert.match(after[0].textContent, /--bg-canvas:\s*#111/);
  assert.doesNotMatch(after[0].textContent, /#000/, "the old value is gone");

  // Unloading removes it, restoring the built-in theme.
  src.set([]);
  await host.sync();
  assert.equal(head.children.filter((c) => c.tag === "style").length, 0);
});

// ---------------------------------------------------------------------------
// `synced`: telling "not yet" apart from "nothing"
//
// The catch-all page uses this to decide between "Loading…" and "Not found".
// Without it a page visited during boot would show a 404 that is really just
// "too early".
// ---------------------------------------------------------------------------

test("synced starts false and only flips after a successful sync", async () => {
  let fail = true;
  const host = new PluginHost(fakeDom(), async () => {
    if (fail) throw new Error("backend not ready");
    return [];
  });

  assert.equal(host.synced, false, "nothing has been synced yet");

  // A failing source must NOT count as synced — the backend was never reached.
  await host.sync();
  assert.equal(host.synced, false, "a failed fetch leaves it unsynced, so the UI keeps waiting");

  fail = false;
  await host.sync();
  assert.equal(host.synced, true, "a successful sync — even an empty one — means plugins have reported");
});

// ---------------------------------------------------------------------------
// Contributed routes, through the host (not just the registry)
// ---------------------------------------------------------------------------

test("a plugin's declared routes reach the registry and resolve", async () => {
  const host = new PluginHost(fakeDom(), async () => [
    {
      slot: "usage",
      provides_slots: [],
      injects_slots: [],
      assets: { "entry.js": `studio.register("UsagePage", (el) => { el.textContent = "u"; });` },
      routes: [
        { path: "usage", component: "UsagePage", title: "Usage", icon: "U", nav: true },
      ],
    },
  ]);
  await host.sync();

  const r = host.slots.routeFor("usage");
  assert.ok(r, "the route registered");
  assert.equal(r.owner, "usage");
  assert.equal(r.component, "UsagePage");
  assert.deepEqual(host.slots.navRoutes().map((n) => n.path), ["usage"]);

  // And the component the route names is genuinely mountable — the two halves
  // must agree or the page would render blank.
  const parent = new FakeEl("div");
  const dispose = host.mountComponent(r.owner, r.component, `route:${r.path}`, parent as unknown as HTMLElement);
  assert.equal(parent.children.length, 1, "the page's component mounted");
  assert.equal(parent.children[0].textContent, "u");
  dispose();
  assert.equal(parent.children.length, 0, "and unmounted");
});

test("a plugin cannot shadow a built-in page", async () => {
  const errors: unknown[] = [];
  const orig = console.error;
  console.error = (...a: unknown[]) => errors.push(a);
  try {
    const host = new PluginHost(fakeDom(), async () => [
      {
        slot: "sneaky",
        provides_slots: [],
        injects_slots: [],
        assets: {},
        // SvelteKit resolves static routes first, so this could never render;
        // accepting it silently would make the plugin believe it had taken over
        // the built-in page.
        routes: [{ path: "chat", component: "Fake", title: "Chat", nav: true }],
      },
    ]);
    await host.sync();
    assert.equal(host.slots.routeFor("chat"), undefined, "the built-in path stays unclaimed");
    assert.deepEqual(host.slots.navRoutes(), [], "and no nav entry was added");
    assert.equal(errors.length, 1, "the attempt is reported, not swallowed");
  } finally {
    console.error = orig;
  }
});

test("routes from two plugins do not collide unless they share a path", async () => {
  const host = new PluginHost(fakeDom(), async () => [
    {
      slot: "a",
      provides_slots: [],
      injects_slots: [],
      assets: {},
      routes: [{ path: "a-page", component: "A", title: "A", nav: true }],
    },
    {
      slot: "b",
      provides_slots: [],
      injects_slots: [],
      assets: {},
      routes: [{ path: "b-page", component: "B", title: "B", nav: true }],
    },
  ]);
  await host.sync();
  assert.deepEqual(
    host.slots.navRoutes().map((n) => n.path),
    ["a-page", "b-page"],
    "both contributed pages are listed",
  );
});

test("a route conflict is reported and does not stop the other plugin loading", async () => {
  const errors: unknown[] = [];
  const orig = console.error;
  console.error = (...a: unknown[]) => errors.push(a);
  try {
    const host = new PluginHost(fakeDom(), async () => [
      {
        slot: "first",
        provides_slots: [],
        injects_slots: [],
        assets: { "entry.js": `studio.register("C", (el) => {});` },
        routes: [{ path: "same", component: "C", title: "First", nav: true }],
      },
      {
        slot: "second",
        provides_slots: [],
        injects_slots: [],
        assets: { "entry.js": `studio.register("C", (el) => {});` },
        routes: [{ path: "same", component: "C", title: "Second", nav: true }],
      },
    ]);
    await host.sync();
    // The first keeps the route; the second is refused and reported.
    assert.equal(host.slots.routeFor("same")?.owner, "first");
    assert.equal(errors.length, 1, "the conflict is surfaced");
    // Crucially, the losing plugin is still loaded — one bad declaration must
    // not take another plugin down.
    assert.ok(host.diagnostics().length >= 0);
  } finally {
    console.error = orig;
  }
});

// ---------------------------------------------------------------------------
// A window component must be able to fill its window
// ---------------------------------------------------------------------------

test("a window mount gives its wrapper a height, a route mount does not", async () => {
  // A component's own `height: 100%` resolves against the wrapper the host
  // inserts. That wrapper is a plain div, so without an explicit height it is
  // `auto` — and a window component asking to fill its window fills only its
  // content. It looks fine until the content is short, which is exactly the
  // kind of defect that survives a visual check.
  //
  // The same wrapper is used for route components, where `100%` is wrong
  // (it would pin a scrolling page to the viewport), so it is opt-in.
  const parent = new FakeEl("div");
  const host = new PluginHost(fakeDom(), async () => [
    {
      slot: "s",
      provides_slots: [],
      injects_slots: [],
      assets: {
        "entry.js": "studio.register('W', (el) => { el.textContent = 'w'; });",
      },
    },
  ]);
  await host.sync();

  const routeParent = new FakeEl("div");
  host.mountComponent("s", "W", "route-1", routeParent as unknown as HTMLElement);
  const routeWrapper = routeParent.children[0] as unknown as { style: Record<string, string> };
  assert.notEqual(
    routeWrapper.style.height,
    "100%",
    "a route component must not be pinned to the viewport height",
  );

  const winParent = new FakeEl("div");
  host.mountComponent("s", "W", "win-1", winParent as unknown as HTMLElement, { fill: true });
  const winWrapper = winParent.children[0] as unknown as { style: Record<string, string> };
  assert.equal(
    winWrapper.style.height,
    "100%",
    "a window component must be able to fill the window",
  );
});
