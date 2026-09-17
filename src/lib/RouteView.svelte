<script lang="ts">
  // Renders a plugin-contributed page full-bleed.
  //
  // A contributed route is a `PluginRoute` (see `$lib/slots`): path, owner and
  // the component name to mount. This component is the mount point — it does
  // not know what the plugin renders, exactly like `<Slot>`.
  //
  // Why not real routes: with `adapter-static` + `ssr = false` the app is an
  // SPA, and plugin paths are only known at RUNTIME (they arrive from WASM).
  // A build-time route cannot exist for them, so one catch-all resolves them.
  import { onDestroy } from "svelte";
  import type { PluginRoute } from "$lib/slots";
  import { pluginHost } from "$lib/plugin-runtime";

  interface Props {
    /** The contributed route to render. */
    route: PluginRoute;
  }
  let { route }: Props = $props();

  let container: HTMLElement | undefined = $state();
  let cleanups: Array<() => void> = [];

  // Re-derive when plugins change, so a hot reload swaps the component without
  // a navigation.
  let revision = $state(0);
  const unsub = pluginHost.subscribe(() => (revision += 1));
  onDestroy(unsub);

  $effect(() => {
    void revision;
    const root = container;
    if (!root) return;
    // Unmount whatever the previous render left behind.
    for (const c of cleanups) c();
    cleanups = [];
    root.replaceChildren();

    // A `mountComponent` key is what makes the mount idempotent and
    // unmountable; including the owner keeps two plugins from colliding on a
    // same-named component.
    const key = `route:${route.owner}:${route.path}`;
    cleanups.push(
      pluginHost.mountComponent(route.owner, route.component, key, root),
    );

    return () => {
      for (const c of cleanups) c();
      cleanups = [];
    };
  });
</script>

<div class="plugin-route" bind:this={container}></div>

<style>
  .plugin-route {
    display: contents;
  }
</style>
