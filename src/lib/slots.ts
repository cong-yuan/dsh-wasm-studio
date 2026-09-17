/**
 * Frontend slot registry — the reactive counterpart of the backend's
 * `provides`/`injects` service graph.
 *
 * A **slot** is a named place in the UI. Plugins can:
 * - **open** a slot for others (`provides`), and
 * - **claim** a place inside a slot (`injects`).
 *
 * The rules (agreed design):
 *
 * 1. **Order-independent** — a claim made before its slot exists is *pending*
 *    and mounts as soon as the slot appears. Loading order never matters.
 * 2. **Reactive** — when a slot disappears (its plugin unloaded), its
 *    contributors are hidden but *remembered*; when the slot returns, they
 *    reappear with no re-registration.
 * 3. **Conflict is an error** — two live plugins may not open the same slot
 *    name. (Multiple contributors into one slot is fine and ordered by
 *    `priority`, then registration order.)
 *
 * The built-in slots are just slots the app itself opens at startup.
 *
 * This module is deliberately pure (no Svelte, no DOM) so the convergence
 * rules can be unit-tested directly.
 */

/** The five built-in mount points the app opens for plugins. */
export const BUILTIN_SLOTS = [
  "sidebar.items",
  "settings.tabs",
  "dashboard.cards",
  "agent.actions",
  "plugin.detail",
] as const;

/** The app's own slot names. Compile-time known, so typos are caught. */
export type BuiltinSlot = (typeof BUILTIN_SLOTS)[number];

declare const PLUGIN_SLOT: unique symbol;

/**
 * A slot name opened by a plugin, at runtime.
 *
 * Branded, so a plain string is **not** silently assignable: a typo like
 * `slot="settings.tab"` fails to compile instead of quietly rendering nothing.
 * Construct one with {@link pluginSlot}:
 *
 * ```ts
 * <Slot slot={pluginSlot(someNameFromBackend)} />
 * ```
 */
export type PluginSlotName = string & { readonly [PLUGIN_SLOT]: true };

/**
 * A slot name in a UI position: a built-in, or a plugin-opened slot.
 *
 * The built-ins are typed, so `<Slot slot="settings.tab" />` is a compile
 * error (verified: the branded form makes `svelte-check` reject the typo). A
 * plugin-opened slot cannot be known at compile time — its name arrives from
 * WASM as data — so it must be passed through {@link pluginSlot}, which is the
 * explicit "I know this is a runtime name" marker.
 *
 * dsh-web solves the same problem with a module-augmented `SlotMap`, which
 * works because their plugins are TypeScript compiled into the same program and
 * the slot names are therefore compile-time constants. WASM plugin names are
 * data, not code, so that trick does not transfer; typing the built-ins and
 * branding the rest is the honest subset.
 */
export type SlotName = BuiltinSlot | PluginSlotName;

/** Mark a string as a plugin-opened slot name (see {@link PluginSlotName}). */
export function pluginSlot(name: string): PluginSlotName {
  return name as PluginSlotName;
}

/** A slot opened by the app or a plugin. */
export interface Slot {
  /** Globally unique name. */
  name: string;
  /** `app` for built-ins, otherwise the plugin (slot) that opened it. */
  owner: string;
  description?: string;
}

/** One plugin's claim on a place inside a slot. */
export interface Contribution {
  /** The plugin slot that made the claim. */
  owner: string;
  /** The slot it wants to render into. */
  slot: string;
  /** Lower renders first. */
  priority: number;
  /** Which registered component to mount (opaque to this module). */
  component?: string;
  /**
   * Set when a `replace` adjustment redirected this contribution: the plugin
   * whose component must be looked up instead of `owner`. Kept separate from
   * `owner` so teardown stays attributed to the original claim.
   */
  renderOwner?: string;
}

/** What an adjustment does to a matched contribution. */
export type AdjustAction =
  | "hide"
  | "unhide"
  | "replace"
  | "priority";

/**
 * One plugin's adjustment to contributions it does not own.
 *
 * `slot` and `from` are globs (`*` wildcard, matched against a slot name and a
 * contribution's owner). Adjustments are applied at **resolution** time, in the
 * order the adjusting plugins loaded — never by mutating another plugin's DOM.
 */
export interface Adjustment {
  /** The plugin applying it (for teardown and conflict reporting). */
  owner: string;
  /** Glob matched against the contribution's slot. `"*"` matches all. */
  slot: string;
  /** Glob matched against the contribution's owner. `undefined` matches all. */
  from?: string;
  action: AdjustAction;
  /** For `priority`: the absolute new priority. */
  to?: number;
  /** For `priority`: a delta on the contribution's own priority. */
  by?: number;
  /** For `replace`: the component name to substitute (registered by `owner`). */
  component?: string;
}

/** A resolved render request handed to the UI. */
export interface Mount {
  slot: string;
  contribution: Contribution;
}

/**
 * The result of resolving one contribution through the adjustment pipeline.
 * `hidden` contributions are dropped from `mounts()`; `replaced` carries the
 * substituting component so the UI knows where to look for the factory.
 */
export interface Resolved {
  contribution: Contribution;
  hidden: boolean;
  /** Set when a `replace` won; the component to render instead. */
  replacedBy?: { owner: string; component: string };
}

/** Glob matching: an exact string, or a `*` anywhere matching any run. */
export function globMatch(pattern: string | undefined, value: string): boolean {
  if (pattern === undefined) return true;
  if (pattern === "*") return true;
  if (!pattern.includes("*")) return pattern === value;
  const rx = new RegExp(
    "^" + pattern.split("*").map((p) => p.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join(".*") + "$",
  );
  return rx.test(value);
}

/** Raised when two live owners open the same slot name. */
export class SlotConflictError extends Error {
  constructor(name: string, a: string, b: string) {
    super(`slot "${name}" is opened by both "${a}" and "${b}"`);
    this.name = "SlotConflictError";
  }
}

export class SlotRegistry {
  /** name -> the owner that opened it. */
  private slots = new Map<string, Slot>();
  /** plugin owner -> the slot names it opened (for precise teardown). */
  private openedBy = new Map<string, Set<string>>();
  /** All claims ever made, including pending ones. */
  private contributions = new Map<string, Contribution>();
  /** Adjustments, insertion-ordered by the plugin that applied them. */
  private adjustments = new Map<string, Adjustment[]>();
  /** Global order in which adjusting plugins first applied (load order). */
  private adjustOrder: string[] = [];
  private nextId = 1;
  /** Bumped on every mutation so the UI can re-derive cheaply. */
  private version = 0;

  constructor() {
    for (const name of BUILTIN_SLOTS) {
      this.slots.set(name, { name, owner: "app" });
    }
  }

  /** Monotonic revision; callers can cache derived state keyed on it. */
  get revision(): number {
    return this.version;
  }

  private bump() {
    this.version++;
  }

  /** Every open slot. */
  listSlots(): Slot[] {
    return [...this.slots.values()];
  }

  /** Open a slot on behalf of `owner`. Throws on a conflicting owner. */
  openSlot(owner: string, name: string, description?: string): void {
    const existing = this.slots.get(name);
    if (existing && existing.owner !== owner) {
      throw new SlotConflictError(name, existing.owner, owner);
    }
    this.slots.set(name, { name, owner, description });
    const set = this.openedBy.get(owner) ?? new Set<string>();
    set.add(name);
    this.openedBy.set(owner, set);
    this.bump();
  }

  /**
   * Record a contribution. May be made before the target slot exists — it stays
   * pending until the slot opens (rule 1).
   */
  claim(
    owner: string,
    slot: string,
    priority = 0,
    component?: string,
  ): string {
    const id = `c${this.nextId++}`;
    this.contributions.set(id, { owner, slot, priority, component });
    this.bump();
    return id;
  }

  /** Remove one claim by id. No-op if it is already gone. */
  unclaim(id: string): void {
    if (this.contributions.delete(id)) this.bump();
  }

  /**
   * Remove everything owned by `owner` (slot opened + all its claims).
   * Claims *into* the owner's slots are kept — they become pending when their
   * target disappears (rule 2), so they reappear if the slot comes back.
   */
  release(owner: string): void {
    for (const [name, slot] of this.slots) {
      if (slot.owner === owner) {
        this.slots.delete(name);
      }
    }
    this.openedBy.delete(owner);
    for (const [id, c] of this.contributions) {
      if (c.owner === owner) {
        this.contributions.delete(id);
      }
    }
    // Adjustments are owned like everything else: dropping them restores the
    // adjusted contributions to their declared state (reversibility).
    if (this.adjustments.delete(owner)) {
      this.adjustOrder = this.adjustOrder.filter((o) => o !== owner);
    }
    this.bump();
  }

  /** Are all of `owner`'s claims currently mountable? */
  isSatisfied(owner: string): boolean {
    for (const c of this.contributions.values()) {
      if (c.owner === owner && !this.slots.has(c.slot)) return false;
    }
    return true;
  }

  /**
   * Resolve every contribution through the adjustment pipeline, in load order
   * of the adjusting plugins. Later adjustments win, so a plugin loaded last
   * has the final say — which is the whole point of the feature.
   */
  resolve(): Resolved[] {
    const out: Resolved[] = [];
    for (const c of this.contributions.values()) {
      if (!this.slots.has(c.slot)) continue;
      out.push(this.applyAdjustments(c));
    }
    return out;
  }

  /** Fold every adjustment, in load order, over one contribution. */
  private applyAdjustments(original: Contribution): Resolved {
    let c: Contribution = { ...original };
    let hidden = false;
    let replacedBy: Resolved["replacedBy"];

    for (const owner of this.adjustOrder) {
      for (const adj of this.adjustments.get(owner) ?? []) {
        if (!globMatch(adj.slot, c.slot)) continue;
        if (!globMatch(adj.from, c.owner)) continue;
        switch (adj.action) {
          case "hide":
            hidden = true;
            break;
          case "unhide":
            hidden = false;
            break;
          case "replace":
            // A `replace` with no component is a no-op rather than a breakage.
            if (adj.component) replacedBy = { owner: adj.owner, component: adj.component };
            break;
          case "priority":
            if (adj.to !== undefined) c.priority = adj.to;
            else if (adj.by !== undefined) c.priority += adj.by;
            break;
        }
      }
    }
    return { contribution: c, hidden, replacedBy };
  }

  /** Replace the adjustments applied by `owner` (idempotent re-sync). */
  setAdjustments(owner: string, list: Adjustment[]): void {
    if (list.length === 0) {
      this.adjustments.delete(owner);
      this.adjustOrder = this.adjustOrder.filter((o) => o !== owner);
    } else {
      if (!this.adjustments.has(owner)) this.adjustOrder.push(owner);
      this.adjustments.set(owner, list);
    }
    this.bump();
  }

  /** Every adjustment currently in effect (for the slot inspector). */
  listAdjustments(): Adjustment[] {
    return this.adjustOrder.flatMap((o) => this.adjustments.get(o) ?? []);
  }

  /**
   * Every claim whose slot currently exists and survives adjustment, in render
   * order. This is the reactive read: slots that vanished simply are not
   * included, and their contributors reappear automatically once the slot
   * returns. Hidden contributions are filtered out; `replace` rewrites which
   * component the UI looks up.
   */
  mounts(): Mount[] {
    const out: Mount[] = [];
    for (const r of this.resolve()) {
      if (r.hidden) continue;
      const c = r.replacedBy
        ? { ...r.contribution, component: r.replacedBy.component, renderOwner: r.replacedBy.owner }
        : r.contribution;
      out.push({ slot: c.slot, contribution: c });
    }
    out.sort(
      (a, b) =>
        a.slot.localeCompare(b.slot) ||
        a.contribution.priority - b.contribution.priority ||
        a.contribution.owner.localeCompare(b.contribution.owner),
    );
    return out;
  }

  /** Mounts for one slot, in order. */
  mountsFor(slot: string): Contribution[] {
    return this.mounts()
      .filter((m) => m.slot === slot)
      .map((m) => m.contribution);
  }

  /** Claims whose target slot does not exist yet (diagnostics). */
  pending(): Contribution[] {
    return [...this.contributions.values()].filter(
      (c) => !this.slots.has(c.slot),
    );
  }
}
