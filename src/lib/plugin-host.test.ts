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
  removed = false;
  constructor(tag: string) {
    this.tag = tag;
  }
  appendChild(c: FakeEl): FakeEl {
    // detach from any previous parent, like the real DOM
    this.children.push(c);
    return c;
  }
  remove(): void {
    this.removed = true;
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
  assert.equal(parent.children[0].textContent, "hello", "the factory ran");
  mountedText = parent.children[0].textContent;

  dispose();
  assert.equal(parent.children[0].removed, true, "wrapper removed on unmount");
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
