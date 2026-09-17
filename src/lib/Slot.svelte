<script lang="ts">
  // Renders every plugin contribution registered into a named slot.
  //
  // Usage:
  //   <Slot slot="settings.tabs" />
  //   <Slot slot="llm-ui.config" />   <!-- a plugin-opened slot -->
  //
  // The component is purely a mount point: it asks the host for the current
  // render list and mounts each contribution's component into a child element.
  // Reactivity comes from the host's `subscribe`, so a slot that appears or
  // disappears (its plugin loading/unloading) re-renders automatically — that
  // is rule 2 of the slot registry, made visible.
  //
  // C2: components are arbitrary JS that receive a DOM element. This component
  // never inspects what they render.
  import { onDestroy } from "svelte";
  import type { Contribution, SlotName } from "$lib/slots";
  import { pluginHost } from "$lib/plugin-runtime";

  interface Props {
    /**
     * The slot to render. Built-in names are typed, so a typo in one is a
     * compile error; plugin-created slot names are arbitrary strings.
     */
    slot: SlotName;
    /** Rendered when the slot has no contributors at all. */
    empty?: boolean;
  }
  let { slot, empty = false }: Props = $props();

  // A revision counter bumped by the host on every change, so Svelte re-derives.
  let revision = $state(0);
  const unsub = pluginHost.subscribe(() => (revision += 1));
  onDestroy(unsub);

  let contributions = $derived.by(() => {
    void revision; // dependency
    return pluginHost.contributionsFor(slot);
  });

  /** One mount point per contribution; the host owns the DOM inside. */
  let container: HTMLElement | undefined = $state();
  let cleanups: Array<() => void> = [];

  $effect(() => {
    const list = contributions;
    const root = container;
    if (!root) return;
    // Re-mount whenever the list or the element changes.
    for (const c of cleanups) c();
    cleanups = list.map((c: Contribution) => pluginHost.mount(c, root));
    return () => {
      for (const c of cleanups) c();
      cleanups = [];
    };
  });
</script>

<div class="plugin-slot" bind:this={container}></div>
{#if empty && contributions.length === 0}
  <div class="faint" style="font-size: 12px; padding: 8px 0;">
    no contributions to <code>{slot}</code>
  </div>
{/if}

<style>
  .plugin-slot {
    display: contents;
  }
</style>
