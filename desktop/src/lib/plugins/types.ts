/**
 * The plugins, as the runtime reports them.
 *
 * A plugin is an MCP server: this build ships a fixed set, the page connects
 * one, and what it offers arrives as discovered tools. These are the bridge's
 * own wire shapes, passed through the host untouched, which is why the fields
 * keep the runtime's snake_case.
 */

/** One tool a connected plugin offers, as the model sees it. */
export interface PluginTool {
  /** The namespaced name the model calls: `mcp__<server>__<tool>`. */
  name: string
  description: string
  /** The host's authorization: `allow` runs, `confirm` asks the user first. */
  policy: 'allow' | 'confirm'
}

/** One plugin that can be connected. */
export interface PluginServer {
  /** The server's name, as `plugins.connect` takes it. */
  name: string
  connected: boolean
  /**
   * The tools discovered from it. Empty until it is connected: a catalogue
   * exists only once the runtime has asked the server for one.
   */
  tools: PluginTool[]
  /** Where it comes from: compiled into this build, or declared by the workspace. */
  source?: 'builtin' | 'workspace'
  /** The command line a declared plugin is started with, when there is one. */
  command?: string | null
}

/** What `plugins.list` answers with: the whole set, and what is connected. */
export interface PluginList {
  available: number
  /** The names connected so far, in stable order. */
  connected: string[]
  servers: PluginServer[]
}

/** What `plugins.connect` answers with: the server, and what happened. */
export interface PluginConnected {
  server: string
  message: string
}
