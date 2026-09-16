/**
 * The single plugin host for the app.
 *
 * Kept as a module-level singleton because plugin registrations are inherently
 * app-global (a slot opened by one plugin is visible to every `<Slot>` in the
 * tree). Pages import `pluginHost` and pass it to `<Slot slot="…" />`.
 */

import { PluginHost } from "./plugin-host.ts";

export const pluginHost = new PluginHost();
