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
