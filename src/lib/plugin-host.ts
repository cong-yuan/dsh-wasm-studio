/**
 * The frontend plugin host — the bridge between WASM plugins and the UI.
 *
 * Responsibilities:
 *  1. keep a {@link SlotRegistry} in sync with the backend's `ui_contributions`;
 *  2. execute each plugin's `entry.js` (C2: arbitrary JS, no sandbox);
 *  3. expose `window.studio` so plugin code can register components, open slots
 *     and claim places in them;
 *  4. tear a plugin's registrations down when it unloads.
 *
 * ## Component model (C2)
 *
 * A plugin's `entry.js` receives a mount target — an ordinary DOM element — and
 * renders however it likes (plain DOM, or its own framework). The host does not
 * interpret the component; it only decides *where* it goes.
 *
 * ```js
 * // plugin entry.js
 * studio.register("ThemePanel", (el, ctx) => {
 *   el.textContent = "hello from " + ctx.slot;
 *   return () => { el.textContent = ""; };   // optional disposer
 * });
 * ```
 *
 * ## Why this is not sandboxed
 *
 * Plugin JS runs in the app's webview with full access to the Tauri API. That is
 * the deliberate "trusted plugin, maximum capability" decision — see the host
 * repo's docs/已知问题.md §1.1.
 */

import {
  SlotConflictError,
  SlotRegistry,
  type Contribution,
} from "./slots.ts";
import { uiContributions, type UiPlugin } from "./api.ts";

/** Where {@link PluginHost.sync} reads the current plugin declarations. */
export type ContributionsSource = () => Promise<UiPlugin[]>;

/** What a component factory receives when the host mounts it. */
export interface MountContext {
  /** The slot being rendered into. */
  slot: string;
  /** The plugin slot that owns this contribution. */
  owner: string;
  /** The component name the plugin registered. */
  component: string;
}

/** Why a contribution is (or is not) rendering. */
export interface ContributionDiagnostic {
  owner: string;
  slot: string;
  component: string | null;
  /** Is the target slot currently open? */
  slotOpen: boolean;
  /** Did the owner register the named component? */
  hasFactory: boolean;
  /** A human-readable verdict. */
  status:
    | "waiting for slot"
    | "no component named"
    | "component not registered"
    | "ready";
}

/** A container a plugin opened for a slot, plus what is currently in it. */
interface SlotTarget {
  slot: string;
  el: HTMLElement;
  holders: Map<string, HTMLElement>;
}

/** A component factory: render into `el`, optionally return a disposer. */
export type ComponentFactory = (
  el: HTMLElement,
  ctx: MountContext,
) => void | (() => void);

/** The API surface handed to plugin `entry.js` files as `window.studio`. */
export interface StudioApi {
  /** Register a component factory under a name. */
  register(name: string, factory: ComponentFactory): void;
  /** Register several at once: `{ Name: factory, … }`. */
  components(map: Record<string, ComponentFactory>): void;
  /**
   * Load one of this plugin's **other** `.js` assets as a module, returning
   * whatever that file `return`s.
   *
   * This is what lets a plugin be more than one file: a shared helper or a
   * component can live in its own asset instead of being concatenated into a
   * single `entry.js` string.
   *
   * ```js
   * // assets: { "lib/format.js": "return { pct: (n) => n + '%' };",
   * //           "entry.js": "const { pct } = studio.require('lib/format'); …" }
   * ```
   *
   * Modules are loaded **lazily** on first `require` (so an unused one never
   * runs), cached per plugin, and may require each other. A cycle throws rather
   * than hanging.
   */
  require(name: string): unknown;
  /** Open a slot for others to fill. Usually declared in the WASM `ui` block. */
  provideSlot(name: string, description?: string): void;
  /** Claim a place inside a slot. Usually declared in the WASM `ui` block. */
  inject(slot: string, component: string, priority?: number): void;
  /**
   * Render the children of a slot **into `el`** — the missing half of opening a
   * slot. A plugin that opens `my.slot` must also call this somewhere in its own
   * UI, or nothing it hosts will ever appear.
   *
   * Returns a disposer; call it when that part of the DOM goes away.
   */
  renderSlot(slot: string, el: HTMLElement): () => void;
  /**
   * Open one of this plugin's declared windows, optionally passing params that
   * the window reads on startup (`studio.windowParams()`).
   *
   * `name` is the plugin-local name from the `windows` declaration.
   */
  openWindow(name: string, params?: unknown): Promise<void>;
  /** Close one of this plugin's windows (no-op if it is not open). */
  closeWindow(name: string): Promise<void>;
  /**
   * The params this window was opened with, or `null` when it is the main
   * window. Read once at component mount.
   */
  windowParams(): unknown;
  /** The current window's label (`"main"` in the main window). */
  windowLabel(): string;
  /** Unregister everything this plugin registered (called on unload). */
  dispose(): void;
}

/**
 * The tiny slice of the DOM the host needs. Injected so the host's logic can
 * be tested under Node (which has no `document`), and so a future non-DOM
 * renderer could be swapped in.
 */
export interface DomAdapter {
  createElement(tag: string): HTMLElement;
  /** The `<head>`, for stylesheet injection. */
  head(): HTMLElement;
}

const browserDom: DomAdapter = {
  createElement: (tag) => document.createElement(tag),
  head: () => document.head,
};

/** Live instances, so DOM nodes can be re-mounted/reacted to. */
interface LiveMount {
  contribution: Contribution;
  factory: ComponentFactory;
  /** The wrapper the factory rendered into, once mounted. */
  el: HTMLElement;
  /** The factory's own disposer, if it returned one. */
  teardown?: () => void;
}

/**
 * Per-plugin registration bookkeeping. Created for each plugin's `entry.js`,
 * and discarded on unload so component factories do not leak.
 */
class PluginHandle {
  /** Registered component factories, by name. */
  readonly factories = new Map<string, ComponentFactory>();
  /** Claim ids this plugin created through the JS API (not the WASM block). */
  readonly jsClaims: string[] = [];
  /** Source of this plugin's non-entry `.js` assets, by module name. */
  readonly moduleSources = new Map<string, string>();
  /** Loaded module exports, by module name. Cleared with the handle. */
  private readonly modules = new Map<string, unknown>();
  /** The source each module was evaluated from, so a change invalidates it. */
  private readonly moduleBodies = new Map<string, string>();
  /** Guards against a require cycle (which would otherwise recurse forever). */
  private readonly loading = new Set<string>();
  /** The `studio` object, needed so a module can itself call `require`. */
  private api: StudioApi | null = null;
  disposed = false;

  private readonly owner: string;
  private readonly reg: SlotRegistry;
  private readonly onChanged: () => void;
  /**
   * Teardowns to run when this handle is replaced or released.
   *
   * Hot-reloading a plugin re-runs its `entry.js`, which builds a **new**
   * handle and disposes the old one. Anything the old handle registered must
   * be undone here or it accumulates on every reload — claims made through
   * `studio.inject` used to leak exactly that way, and slot containers kept
   * their refreshers alive.
   */
  private readonly teardowns: Array<{ label: string; run: () => void }> = [];

  constructor(owner: string, reg: SlotRegistry, onChanged: () => void) {
    this.owner = owner;
    this.reg = reg;
    this.onChanged = onChanged;
  }

  /** Register a teardown, labelled so a failure names its source. */
  track(label: string, run: () => void): void {
    this.teardowns.push({ label, run });
  }

  /** The API handed to modules (set once the handle's own `api` exists). */
  bindApi(api: StudioApi): void {
    this.api = api;
  }

  /**
   * Replace the module table, **invalidating any module whose body changed**.
   *
   * A rebuild can touch a helper without touching `entry.js`. The entry.js
   * source is what gates re-execution, so without this the old helper would
   * stay cached and the edit would appear to do nothing — a confusing class of
   * "I changed the code and nothing happened".
   */
  setModuleSources(next: Map<string, string>): void {
    this.moduleSources.clear();
    for (const [k, v] of next) {
      this.moduleSources.set(k, v);
      if (this.moduleBodies.get(k) !== v) {
        this.modules.delete(k);
        this.moduleBodies.delete(k);
      }
    }
    // Drop modules the plugin no longer ships.
    for (const k of [...this.moduleBodies.keys()]) {
      if (!next.has(k)) {
        this.modules.delete(k);
        this.moduleBodies.delete(k);
      }
    }
  }

  /**
   * Load a module asset on demand. Modules may require each other, so this is
   * lazy rather than a fixed order — which also means an unused module costs
   * nothing and cannot break the plugin.
   */
  require(name: string): unknown {
    // Asset keys carry the extension (`lib/format.js`); callers usually omit it
    // (`require("lib/format")`). Accept both, but remember the resolved key so
    // the cache is keyed consistently.
    const resolved = this.moduleSources.has(name) ? name : `${name}.js`;
    if (this.modules.has(resolved)) return this.modules.get(resolved);
    const src = this.moduleSources.get(resolved);
    if (src === undefined) {
      const available = [...this.moduleSources.keys()].sort().join(", ");
      throw new Error(
        `no module "${name}" in plugin "${this.owner}"` +
          (available ? ` (available: ${available})` : " (the plugin ships no other .js assets)"),
      );
    }
    name = resolved;
    if (this.loading.has(name)) {
      throw new Error(`circular require of module "${name}" in plugin "${this.owner}"`);
    }
    this.loading.add(name);
    try {
      // Same shape as entry.js: a module is a function body that may `return`
      // its exports.
      const fn = new Function("studio", src);
      const exports = fn(this.api);
      this.modules.set(name, exports);
      this.moduleBodies.set(name, src);
      return exports;
    } finally {
      this.loading.delete(name);
    }
  }

  register(name: string, factory: ComponentFactory): void {
    this.factories.set(name, factory);
    this.onChanged();
  }

  /** The `components()` bulk form of {@link register}. */
  registerMany(map: Record<string, ComponentFactory>): void {
    for (const [k, v] of Object.entries(map)) this.register(k, v);
  }

  /**
   * Containers a plugin asked us to render a slot into. Kept so a re-sync (a
   * new contributor arriving) can refresh them without the plugin re-rendering.
   */
  readonly slotTargets: SlotTarget[] = [];

  provideSlot(name: string, description?: string): void {
    this.reg.openSlot(this.owner, name, description);
    this.onChanged();
  }

  inject(slot: string, component: string, priority = 0): void {
    const id = this.reg.claim(this.owner, slot, priority, component);
    this.jsClaims.push(id);
    // Recorded here as well as in `jsClaims` so a teardown re-reports it once.
    this.track(`claim ${id} (${slot})`, () => this.reg.unclaim(id));
    this.onChanged();
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    // Run teardowns newest-first, and keep going if one throws: a single bad
    // teardown must not strand the rest (they are independent registrations).
    for (const { label, run } of this.teardowns.splice(0).reverse()) {
      try {
        run();
      } catch (e) {
        console.error(`[studio] teardown of ${this.owner} (${label}) threw:`, e);
      }
    }
    this.jsClaims.length = 0;
    this.factories.clear();
    this.slotTargets.length = 0;
    this.modules.clear();
    this.moduleBodies.clear();
  }
}

export class PluginHost {
  readonly slots = new SlotRegistry();
  private readonly dom: DomAdapter;
  /** owner -> the factory map + JS-registered claims. */
  private handles = new Map<string, PluginHandle>();
  /** Currently mounted DOM, keyed by contribution identity. */
  private live = new Map<string, LiveMount>();
  private listeners = new Set<() => void>();
  /**
   * A signature of the assets we last executed, so we don't re-run unchanged
   * ones. Covers `entry.js` **and** its modules: a helper can change without
   * `entry.js` changing, and the plugin's registrations close over whatever the
   * modules returned — so a module edit must re-run the entry too, or the edit
   * would silently do nothing.
   */
  private executed = new Map<string, string>();
  /** Claim ids created from each plugin's *declarative* `injects`, so a re-sync
   *  replaces them instead of accumulating duplicates. */
  private declClaims = new Map<string, string[]>();
  /** Refreshers for plugin-opened slot containers (re-run on every change). */
  private slotRefreshers = new Set<() => void>();

  private readonly source: ContributionsSource;

  constructor(
    dom: DomAdapter = browserDom,
    source: ContributionsSource = uiContributions,
  ) {
    this.dom = dom;
    this.source = source;
  }

  /** Subscribe to change notifications (for a Svelte `$state` bridge). */
  subscribe(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  private notify() {
    for (const fn of this.listeners) fn();
  }

  /** The `window.studio` object for the *currently executing* entry.js. */
  private currentApi: StudioApi | null = null;

  /**
   * Sync from the backend. Idempotent: safe to call on every change event.
   *
   * - plugins that disappeared are released (rule 2 hides their contributors)
   * - new plugins have their `entry.js` executed once
   * - slot/claim declarations are (re)applied — reopening an owner's own slot
   *   is idempotent, so a plugin that stays loaded is unaffected
   */
  async sync(): Promise<void> {
    let plugins: UiPlugin[];
    try {
      plugins = await this.source();
    } catch {
      return; // backend not ready; the next event will retry
    }

    const seen = new Set<string>();
    for (const p of plugins) {
      seen.add(p.slot);
      // --- registrations (declarative, from the WASM ui block) ---
      for (const name of p.provides_slots) {
        try {
          this.slots.openSlot(p.slot, name);
        } catch (e) {
          if (e instanceof SlotConflictError) {
            console.error(`[studio] ${e.message}`);
          } else throw e;
        }
      }
      // --- entry.js (imperative, may register components) ---
      //
      // Any other `.js` asset is a **module** the entry.js (or another module)
      // can pull in with `studio.require(name)`. Re-running an entry.js builds
      // a fresh handle, so the module table is rebuilt with it — a changed
      // helper is picked up, and stale modules cannot survive a reload.
      const js = p.assets["entry.js"];
      const moduleSources = new Map<string, string>();
      for (const [name, src] of Object.entries(p.assets)) {
        if (name.endsWith(".js") && name !== "entry.js") moduleSources.set(name, src);
      }
      if (js) {
        const signature = assetSignature(js, moduleSources);
        if (this.executed.get(p.slot) !== signature) {
          this.executed.set(p.slot, signature);
          this.runEntry(p, js, moduleSources);
        }
      }
      // --- declarative claims, after entry.js so its components exist ---
      //
      // `sync` is called on every backend change event, so it must be
      // idempotent: drop this plugin's previous declarative claims before
      // re-adding them, or they would multiply on each sync.
      for (const id of this.declClaims.get(p.slot) ?? []) {
        this.slots.unclaim(id);
      }
      const ids: string[] = [];
      for (const i of p.injects_slots) {
        ids.push(
          this.slots.claim(p.slot, i.slot, i.priority, i.component ?? undefined),
        );
      }
      this.declClaims.set(p.slot, ids);
      // --- adjustments (a later plugin reshaping earlier UI) ---
      // Also idempotent: re-sync replaces this plugin's list wholesale.
      this.slots.setAdjustments(
        p.slot,
        (p.adjusts ?? []).map((a) => ({
          owner: p.slot,
          slot: a.slot ?? "*",
          from: a.from,
          action: a.action,
          to: a.to,
          by: a.by,
          component: a.component,
        })),
      );
      // --- styles ---
      const css = p.assets["style.css"];
      if (css) this.ensureStyle(p.slot, css);
    }

    // Release plugins that are gone.
    for (const owner of [...this.handles.keys()]) {
      if (!seen.has(owner)) this.release(owner);
    }

    // A plugin that *opens* a slot must show its children; refresh every such
    // container now that slots/claims may have changed.
    this.refreshPluginSlots();
    this.notify();
  }

  /** Execute one plugin's entry.js with its own `studio` handle. */
  private runEntry(
    p: UiPlugin,
    source: string,
    moduleSources: Map<string, string> = new Map(),
  ): void {
    // Re-running an entry.js replaces the old handle entirely.
    this.handles.get(p.slot)?.dispose();
    const handle = new PluginHandle(p.slot, this.slots, () => this.notify());
    for (const [k, v] of moduleSources) handle.moduleSources.set(k, v);
    this.handles.set(p.slot, handle);

    const api: StudioApi = {
      register: (n, f) => handle.register(n, f),
      components: (m) => handle.registerMany(m),
      require: (n) => handle.require(n),
      provideSlot: (n, d) => handle.provideSlot(n, d),
      inject: (s, c, pr) => handle.inject(s, c, pr),
      renderSlot: (name, el) => this.renderSlotInto(handle, name, el),
      // A plugin names its windows locally; the host owns the namespacing.
      openWindow: async (name, params) => {
        const { openPluginWindowWith } = await import("./api.ts");
        await openPluginWindowWith(pluginLabel(p.slot, name), params ?? null);
      },
      closeWindow: async (name) => {
        const { closePluginWindow } = await import("./api.ts");
        await closePluginWindow(pluginLabel(p.slot, name));
      },
      windowParams: () =>
        (globalThis as { __STUDIO_WINDOW__?: { params?: unknown } }).__STUDIO_WINDOW__
          ?.params ?? null,
      windowLabel: () =>
        (globalThis as { __STUDIO_WINDOW__?: { label?: string } }).__STUDIO_WINDOW__
          ?.label ?? "main",
      dispose: () => handle.dispose(),
    };
    // Modules receive the same api, so `studio.require` works inside them too.
    handle.bindApi(api);
    this.currentApi = api;
    try {
      // C2: arbitrary JS. Run it as a function so a top-level `return` is legal
      // and so it cannot see the host's own scope except through `studio`.
      const fn = new Function("studio", source);
      fn(api);
    } catch (e) {
      console.error(`[studio] plugin "${p.slot}" entry.js failed:`, e);
    } finally {
      this.currentApi = null;
    }
  }

  private styles = new Map<string, HTMLElement>();
  private ensureStyle(owner: string, css: string): void {
    const existing = this.styles.get(owner);
    if (existing) {
      existing.textContent = css;
      return;
    }
    const el = this.dom.createElement("style");
    el.dataset.pluginStyle = owner;
    this.dom.head().appendChild(el);
    el.textContent = css;
    this.styles.set(owner, el);
  }


  /** Remove everything a plugin registered (slot, claims, components, style). */
  release(owner: string): void {
    // Tear down any live DOM first.
    for (const [key, m] of [...this.live]) {
      if (m.contribution.owner === owner) {
        this.unmount(key);
      }
    }
    this.handles.get(owner)?.dispose();
    this.handles.delete(owner);
    this.executed.delete(owner);
    this.declClaims.delete(owner);
    this.slots.release(owner);
    this.styles.get(owner)?.remove();
    this.styles.delete(owner);
    this.notify();
  }

  /**
   * Render the children of `slot` into `el`, and keep them in sync.
   *
   * This is how a plugin that **opens** a slot displays what others contribute.
   * The returned disposer detaches everything and forgets the container.
   */
  renderSlotInto(handle: PluginHandle, slot: string, el: HTMLElement): () => void {
    const target: SlotTarget = { slot, el, holders: new Map() };
    handle.slotTargets.push(target);
    const refresh = () => this.refreshSlotTarget(target);
    refresh();
    // Remember how to refresh, so a later change re-renders this container.
    this.slotRefreshers.add(refresh);
    // Returned to the plugin, which may call it when its DOM goes away. Also
    // registered on the handle, so a **hot reload** detaches the refresher even
    // if the plugin never called its disposer (it was replaced, not torn down).
    const detach = () => {
      this.slotRefreshers.delete(refresh);
      const i = handle.slotTargets.indexOf(target);
      if (i >= 0) handle.slotTargets.splice(i, 1);
      el.replaceChildren();
    };
    handle.track(`slot container ${slot}`, detach);
    return detach;
  }

  /** Re-render every plugin-opened slot container (called after a change). */
  private refreshPluginSlots(): void {
    for (const target of this.allSlotTargets()) this.refreshSlotTarget(target);
  }

  private allSlotTargets(): SlotTarget[] {
    const out: SlotTarget[] = [];
    for (const h of this.handles.values()) out.push(...h.slotTargets);
    return out;
  }

  private refreshSlotTarget(target: SlotTarget): void {
    const wanted = this.slots.mountsFor(target.slot);
    const keyOf = (c: Contribution) => `${c.owner}:${c.slot}:${c.component ?? ""}`;
    const wantedKeys = new Set(wanted.map(keyOf));

    // Drop contributions that no longer belong (owner unloaded, claim gone).
    for (const [key, holder] of [...target.holders]) {
      if (!wantedKeys.has(key)) {
        this.unmount(key);
        holder.remove();
        target.holders.delete(key);
      }
    }
    // Add the new ones.
    for (const c of wanted) {
      const key = keyOf(c);
      if (target.holders.has(key)) continue;
      const holder = this.dom.createElement("div");
      holder.dataset.contribution = key;
      target.el.appendChild(holder);
      this.mount(c, holder);
      target.holders.set(key, holder);
    }
  }

  /**
   * The component factory for a contribution.
   *
   * When a `replace` adjustment redirected it, the factory belongs to the
   * *adjusting* plugin (`renderOwner`), not the original claimant — that is
   * how a later plugin substitutes its own component for an earlier one.
   */
  factoryFor(c: Contribution): ComponentFactory | undefined {
    if (!c.component) return undefined;
    const owner = c.renderOwner ?? c.owner;
    return this.handles.get(owner)?.factories.get(c.component);
  }

  /** Look up a component a plugin registered, by owner + name. */
  factoryForComponent(owner: string, name: string): ComponentFactory | undefined {
    return this.handles.get(owner)?.factories.get(name);
  }

  /**
   * Mount a plugin component **outside** the slot system — used by plugin
   * windows, where a component is rendered full-window rather than into a slot.
   * `key` identifies the instance so it can be unmounted later.
   */
  mountComponent(
    owner: string,
    name: string,
    key: string,
    el: HTMLElement,
  ): () => void {
    const factory = this.factoryForComponent(owner, name);
    if (!factory) return () => {};
    const existing = this.live.get(key);
    if (existing) return () => this.unmount(key);

    const wrapper = this.dom.createElement("div");
    wrapper.dataset.plugin = owner;
    wrapper.dataset.component = name;
    el.appendChild(wrapper);
    // Same containment as `mount`: a window component is plugin JS too, and a
    // throw must not leave an orphaned wrapper behind.
    let teardown: (() => void) | undefined;
    try {
      teardown =
        factory(wrapper, { slot: `window:${name}`, owner, component: name }) ?? undefined;
    } catch (e) {
      console.error(`[studio] window component \"${name}\" from \"${owner}\" threw:`, e);
      wrapper.remove();
      return () => {};
    }
    this.live.set(key, {
      contribution: { owner, slot: `window:${name}`, priority: 0, component: name },
      factory,
      el: wrapper,
      teardown,
    });
    return () => this.unmount(key);
  }

  /** The current render list for one slot (reactive read). */
  contributionsFor(slot: string): Contribution[] {
    return this.slots.mountsFor(slot);
  }

  /**
   * A diagnostic view of every contribution a plugin declared, and why it may
   * not be visible. This is what turns "I don't see it" into a specific cause:
   * the target slot is missing, or the component was never registered.
   */
  diagnostics(): ContributionDiagnostic[] {
    const out: ContributionDiagnostic[] = [];
    // Reuse the registry's own mount list plus its pending set, so we cover
    // both mounted and waiting contributions.
    const all = [...this.slots.mounts().map((m) => m.contribution), ...this.slots.pending()];
    for (const c of all) {
      const slotOpen = this.slots.listSlots().some((s) => s.name === c.slot);
      const factory = this.factoryFor(c);
      out.push({
        owner: c.owner,
        slot: c.slot,
        component: c.component ?? null,
        slotOpen,
        hasFactory: !!factory,
        status: !slotOpen
          ? "waiting for slot"
          : !c.component
            ? "no component named"
            : !factory
              ? "component not registered"
              : "ready",
      });
    }
    out.sort((a, b) => a.owner.localeCompare(b.owner) || a.slot.localeCompare(b.slot));
    return out;
  }

  // -------------------------------------------------------------------------
  // DOM mounting
  // -------------------------------------------------------------------------

  /**
   * Mount a contribution's component into `el`. Idempotent per contribution:
   * calling twice for the same contribution is a no-op, and a component whose
   * factory vanishes is unmounted.
   *
   * Returns a disposer the caller (the `<Slot>` component) must invoke on
   * unmount.
   */
  mount(c: Contribution, el: HTMLElement): () => void {
    const key = `${c.owner}:${c.slot}:${c.component ?? ""}`;
    const factory = this.factoryFor(c);
    if (!factory) return () => {};

    const existing = this.live.get(key);
    if (existing) return () => this.unmount(key);

    const ctx: MountContext = {
      slot: c.slot,
      owner: c.owner,
      component: c.component ?? "",
    };
    const wrapper = this.dom.createElement("div");
    wrapper.dataset.plugin = c.owner;
    wrapper.dataset.slot = c.slot;
    if (c.renderOwner) wrapper.dataset.renderedBy = c.renderOwner;
    el.appendChild(wrapper);

    // A plugin component is arbitrary JS and may throw. Contain it:
    //  * the error must not escape — a `<Slot>` mounts contributions in a loop,
    //    and one throwing factory would otherwise abort the loop and leave
    //    every later contribution in that slot unrendered;
    //  * the wrapper must not be orphaned — remove it and report no live mount,
    //    so the slot renders the rest and the failure is visible as a gap.
    let teardown: (() => void) | undefined;
    try {
      teardown = factory(wrapper, ctx) ?? undefined;
    } catch (e) {
      console.error(
        `[studio] component "${ctx.component}" from "${c.owner}" threw while mounting:`,
        e,
      );
      wrapper.remove();
      return () => {};
    }
    this.live.set(key, { contribution: c, factory, el: wrapper, teardown });

    return () => this.unmount(key);
  }

  /** Tear one mounted contribution down. */
  unmount(key: string): void {
    const m = this.live.get(key);
    if (!m) return;
    try {
      m.teardown?.();
    } catch (e) {
      console.error(`[studio] teardown for ${key} threw:`, e);
    }
    m.el.remove();
    this.live.delete(key);
  }

  /** Unmount everything (used on app teardown / tests). */
  unmountAll(): void {
    for (const key of [...this.live.keys()]) this.unmount(key);
  }
}

/// Mirror of the Rust `Studio::window_label` sanitiser, so a plugin can open a
/// window by its *local* name and get the same label the backend derived.
function pluginLabel(slot: string, name: string): string {
  const safe = (s: string) => s.replace(/[^a-zA-Z0-9_-]/g, "-");
  return `plugin-${safe(slot)}-${safe(name)}`;
}


/**
 * A cheap signature over a plugin's executable assets: `entry.js` plus every
 * module it ships. Any change re-runs the entry, because the registrations it
 * makes close over whatever those modules returned.
 */
function assetSignature(entry: string, modules: Map<string, string>): string {
  const names = [...modules.keys()].sort();
  return [entry, ...names.map((n) => `${n}\u0000${modules.get(n)}`)].join("\u0001");
}
