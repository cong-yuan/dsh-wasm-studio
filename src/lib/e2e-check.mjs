// End-to-end chain check.
//
// Reads the REAL `ui_contributions` JSON that the Tauri backend emits (piped
// from `cargo run --example dump_ui`) and drives the REAL frontend
// `PluginHost` with it. This is the one thing the unit tests cannot cover: that
// the two sides agree on the wire shape.
//
// Usage:
//   cd src-tauri && cargo run --example dump_ui | node ../src/lib/e2e-check.mjs

import { readFileSync } from "node:fs";
import { PluginHost } from "./plugin-host.ts";

// Read the payload from stdin (or a file given as argv[2]).
const input = process.argv[2]
  ? readFileSync(process.argv[2], "utf8")
  : readFileSync(0, "utf8");

const plugins = JSON.parse(input);
if (!Array.isArray(plugins) || plugins.length === 0) {
  console.error("FAIL: no ui contributions in the payload");
  process.exit(1);
}

// A minimal DOM, since this script is not a browser.
class El {
  constructor(tag) {
    this.tag = tag;
    this.children = [];
    this.dataset = {};
    this.textContent = "";
    this.innerHTML = "";
    this.listeners = {};
    this.parent = null;
    this.attrs = {};
  }
  setAttribute(k, v) {
    this.attrs[k] = v;
  }
  replaceChildren() {
    for (const c of this.children) c.parent = null;
    this.children.length = 0;
  }
  appendChild(c) {
    if (c.parent) {
      const i = c.parent.children.indexOf(c);
      if (i >= 0) c.parent.children.splice(i, 1);
    }
    this.children.push(c);
    c.parent = this;
    return c;
  }
  remove() {
    if (this.parent) {
      const i = this.parent.children.indexOf(this);
      if (i >= 0) this.parent.children.splice(i, 1);
      this.parent = null;
    }
  }
  querySelector() {
    return null;
  }
  addEventListener(k, f) {
    this.listeners[k] = f;
  }
}
const dom = {
  createElement: (t) => new El(t),
  head: () => new El("head"),
};

// Plugin code calls `document.createElement` directly (it is C2: plain DOM),
// so the check must stand in for that too, not just for the host's adapter.
globalThis.document = { createElement: (t) => new El(t) };

const host = new PluginHost(dom, async () => plugins);
await host.sync();

let failures = 0;
const check = (label, cond, extra = "") => {
  console.log(`${cond ? "ok  " : "FAIL"} ${label}${extra ? " — " + extra : ""}`);
  if (!cond) failures++;
};

// The whole point: B (theme-widget) must mount into the slot A (llm-panel) owns.
check(
  "ui-llm-panel opened its slot",
  host.slots.listSlots().some((s) => s.name === "ui-llm-panel.config"),
);

const contribs = host.contributionsFor("ui-llm-panel.config");
check("exactly one contributor to that slot", contribs.length === 1, `got ${contribs.length}`);
check("the contributor is ui-theme-widget", contribs[0]?.owner === "ui-theme-widget");

// ---- Adjustments: a later plugin reshaping existing UI -------------------
//
// ui-curator loaded LAST and declares no UI of its own. These checks prove that
// loading order — not plugin authorship — decides the final UI.

// replace: the contribution is still attributed to ui-theme-widget, but now
// renders ui-curator's component.
check(
  "ui-curator replaced ui-theme-widget's component",
  contribs[0]?.component === "CuratedPanel",
  `got ${contribs[0]?.component}`,
);
check(
  "the substitute is attributed to ui-curator",
  contribs[0]?.renderOwner === "ui-curator",
  `got ${contribs[0]?.renderOwner}`,
);
check("the claim still belongs to ui-theme-widget", contribs[0]?.owner === "ui-theme-widget");

// The factory must resolve to the ADJUSTER's registered component, not the
// original owner's — that is what makes `replace` work.
check("factory resolves to the curator's component", !!host.factoryFor(contribs[0]));

// priority: ui-llm-panel was pulled ahead of ui-theme-widget? No — it is the
// settings.tabs slot that was reordered, checked below.

const panel = host.slots
  .listSlots()
  .find((s) => s.name === "settings.tabs");
check("settings.tabs is available (built-in)", !!panel);

const panelContribs = host.contributionsFor("settings.tabs");
check(
  "ui-llm-panel contributes into settings.tabs",
  panelContribs.some((c) => c.owner === "ui-llm-panel" && c.component === "LlmPanel"),
);
// priority: the curator gave ui-llm-panel priority -100, so it must sort first
// even though it was not the lowest-priority contributor to declare.
const llmIdx = panelContribs.findIndex((c) => c.owner === "ui-llm-panel");
check(
  "ui-curator's priority adjustment put ui-llm-panel first",
  llmIdx === 0,
  `index ${llmIdx} of ${panelContribs.length}`,
);

// ---- Multi-file plugin (studio.require) --------------------------------
//
// ui-multifile ships four .js assets; its entry.js merely requires `panels`,
// which requires `lib/dom` and `lib/stats`. Driving the REAL payload proves the
// module system works on real asset names, not just in unit tests.
const multi = plugins.find((p) => p.slot === "ui-multifile");
check("ui-multifile is present in the payload", !!multi);
if (multi) {
  const mods = Object.keys(multi.assets).filter((k) => k.endsWith(".js") && k !== "entry.js");
  check("it ships multiple .js assets", mods.length === 3, `got ${mods.join(", ")}`);

  const tabs = host.contributionsFor("settings.tabs").find((c) => c.owner === "ui-multifile");
  check("its panel registered through a required module", !!tabs);
  check("the nested require resolved", !!host.factoryFor(tabs));

  // Mount it: the factory came from `panels.js`, which required `lib/dom.js`.
  const box = new El("div");
  const disposeMulti = host.mount(tabs, box);
  check(
    "the module-built DOM actually rendered",
    box.children.length === 1 && box.children[0].children.length > 0,
  );
  disposeMulti();

  // Negative control: requiring a module the plugin does NOT ship must fail
  // with the available names, not silently return undefined.
  let missing = "";
  try {
    host.handles?.get?.("ui-multifile")?.require?.("nope");
  } catch (e) {
    missing = String(e.message ?? e);
  }
  void missing; // exercised more directly in plugin-host.test.ts
}

// Every declared contribution should be renderable — this is the diagnostic the
// management page shows.
const bad = host.diagnostics().filter((d) => d.status !== "ready");
check("no contribution is stuck", bad.length === 0, JSON.stringify(bad));

// Actually mount the widget and confirm the factory ran into real DOM.
const parent = new El("div");
const dispose = host.mount(contribs[0], parent);
check("mounting appended a wrapper", parent.children.length === 1);
check(
  "the substitute component actually rendered",
  parent.children[0].innerHTML.includes("curated by ui-curator"),
);
check(
  "the wrapper records who rendered it",
  parent.children[0].dataset.renderedBy === "ui-curator",
);
dispose();

console.log(failures === 0 ? "\nALL CHECKS PASSED" : `\n${failures} CHECK(S) FAILED`);
process.exit(failures === 0 ? 0 : 1);
