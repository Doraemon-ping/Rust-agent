/**
 * The plugins, as the page sees them.
 *
 * One of the seam modules -- alongside `lib/runtime`, `lib/chat`, `lib/cron`,
 * `lib/skills`, `lib/projects`, and `lib/provider` -- which are the only files
 * that import `@tauri-apps/api`. A plugin is named by the server this build
 * ships; an unknown name is refused where the servers are, so the page cannot
 * reach a transport it was not given.
 */

import { invoke } from '@tauri-apps/api/core'

import { requireShell } from '@/lib/shell'
import type { PluginConnected, PluginList } from './types'

/**
 * Every plugin this build ships, with the tools of the ones connected.
 *
 * The registry is the runtime's own, the same one the model's tool pool reads,
 * so this is what the runtime has installed rather than a copy the page keeps.
 */
export async function fetchPlugins(): Promise<PluginList> {
  requireShell()
  return invoke<PluginList>('plugins_list')
}

/**
 * Connect a plugin, discovering its tools.
 *
 * The name must be one the runtime listed; a refusal comes back with the
 * runtime's own words rather than a connection that was never made.
 */
export async function connectPlugin(server: string): Promise<PluginConnected> {
  requireShell()
  return invoke<PluginConnected>('plugins_connect', { server })
}

export type { PluginConnected, PluginList, PluginServer, PluginTool } from './types'
