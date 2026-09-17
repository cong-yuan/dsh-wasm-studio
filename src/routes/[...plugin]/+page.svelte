<script lang="ts">
  // Catch-all for **plugin-contributed routes**.
  //
  // SvelteKit resolves static routes first, so every built-in page
  // (`/chat`, `/settings`, …) still wins and this only sees paths no built-in
  // claims. That is exactly the contract plugins get: they add pages, they
  // cannot shadow the app's own.
  //
  // Plugin paths are runtime data (they come from WASM), so no build-time route
  // can exist for them — with `adapter-static` + `ssr = false` the app is an SPA
  // and this single catch-all resolves them all.
  import { onDestroy } from "svelte";
  import { page } from "$app/state";
  import { pluginHost } from "$lib/plugin-runtime";
  import { normalizePath } from "$lib/slots";
  import RouteView from "$lib/RouteView.svelte";

  // A revision so the lookup re-runs when plugins load or unload.
  let revision = $state(0);
  const unsub = pluginHost.subscribe(() => (revision += 1));
  onDestroy(unsub);

  let path = $derived(normalizePath(page.url.pathname));
  let route = $derived.by(() => {
    void revision;
    return pluginHost.slots.routeFor(path);
  });

  // Distinguish "plugins have not reported yet" from "nothing contributes
  // this", so a cold boot does not look like a 404.
  let pending = $derived.by(() => {
    void revision;
    return !route && !pluginHost.synced;
  });
</script>

{#if route}
  <div class="page-head">
    <div>
      <h1>{route.title ?? route.path}</h1>
      <div class="sub">
        contributed by <span class="mono">{route.owner}</span>
      </div>
    </div>
  </div>
  <RouteView {route} />
{:else if pending}
  <div class="page-head">
    <div>
      <h1>Loading…</h1>
      <div class="sub">waiting for plugins to report their pages</div>
    </div>
  </div>
{:else}
  <div class="page-head">
    <div>
      <h1>Not found</h1>
      <div class="sub">
        no plugin contributes <span class="mono">/{path}</span>
      </div>
    </div>
  </div>
  <div class="empty" style="text-align: left;">
    Built-in pages are not affected — those resolve before this route. A plugin
    page appears here once the plugin declaring it is running.
  </div>
{/if}
