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
  /** Open a slot for others to fill. Usually declared in the WASM `ui` block. */
  provideSlot(name: string, description?: string): void;
  /** Claim a place inside a slot. Usually declared in the WASM `ui` block. */
  inject(slot: string, component: string, priority?: number): void;
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
  disposed = false;

  private readonly owner: string;
  private readonly reg: SlotRegistry;
  private readonly onChanged: () => void;

  constructor(owner: string, reg: SlotRegistry, onChanged: () => void) {
    this.owner = owner;
    this.reg = reg;
    this.onChanged = onChanged;
  }

  register(name: string, factory: ComponentFactory): void {
    this.factories.set(name, factory);
    this.onChanged();
  }

  /** The `components()` bulk form of {@link register}. */
  registerMany(map: Record<string, ComponentFactory>): void {
    for (const [k, v] of Object.entries(map)) this.register(k, v);
  }

  provideSlot(name: string, description?: string): void {
    this.reg.openSlot(this.owner, name, description);
    this.onChanged();
  }

  inject(slot: string, component: string, priority = 0): void {
    this.jsClaims.push(this.reg.claim(this.owner, slot, priority, component));
    this.onChanged();
  }

  dispose(): void {
    this.disposed = true;
    this.factories.clear();
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
  /** The last asset sources we executed, so we don't re-run unchanged ones. */
  private executed = new Map<string, string>();
  /** Claim ids created from each plugin's *declarative* `injects`, so a re-sync
   *  replaces them instead of accumulating duplicates. */
  private declClaims = new Map<string, string[]>();

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
      const js = p.assets["entry.js"];
      if (js && this.executed.get(p.slot) !== js) {
        this.executed.set(p.slot, js);
        this.runEntry(p, js);
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
      // --- styles ---
      const css = p.assets["style.css"];
      if (css) this.ensureStyle(p.slot, css);
    }

    // Release plugins that are gone.
    for (const owner of [...this.handles.keys()]) {
      if (!seen.has(owner)) this.release(owner);
    }
    this.notify();
  }

  /** Execute one plugin's entry.js with its own `studio` handle. */
  private runEntry(p: UiPlugin, source: string): void {
    // Re-running an entry.js replaces the old handle entirely.
    this.handles.get(p.slot)?.dispose();
    const handle = new PluginHandle(p.slot, this.slots, () => this.notify());
    this.handles.set(p.slot, handle);

    const api: StudioApi = {
      register: (n, f) => handle.register(n, f),
      components: (m) => handle.registerMany(m),
      provideSlot: (n, d) => handle.provideSlot(n, d),
      inject: (s, c, pr) => handle.inject(s, c, pr),
      dispose: () => handle.dispose(),
    };
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

  /** The component factory for a contribution, if the owner registered one. */
  factoryFor(c: Contribution): ComponentFactory | undefined {
    if (!c.component) return undefined;
    return this.handles.get(c.owner)?.factories.get(c.component);
  }

  /** The current render list for one slot (reactive read). */
  contributionsFor(slot: string): Contribution[] {
    return this.slots.mountsFor(slot);
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
    el.appendChild(wrapper);
    const teardown = factory(wrapper, ctx) ?? undefined;
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
