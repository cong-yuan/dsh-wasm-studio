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
}

/** A resolved render request handed to the UI. */
export interface Mount {
  slot: string;
  contribution: Contribution;
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
   * Every claim whose slot currently exists, in render order.
   * This is the reactive read: slots that vanished simply are not included,
   * and their contributors reappear automatically once the slot returns.
   */
  mounts(): Mount[] {
    const out: Mount[] = [];
    for (const c of this.contributions.values()) {
      if (this.slots.has(c.slot)) {
        out.push({ slot: c.slot, contribution: c });
      }
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
