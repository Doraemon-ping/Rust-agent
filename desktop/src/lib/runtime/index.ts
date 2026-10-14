/**
 * The only place in the app that talks to the host.
 *
 * Views and stores call these functions; nothing else imports `@tauri-apps/api`.
 * That keeps the boundary auditable: there is exactly one file that can start,
 * stop, or query the runtime, and no path from the UI to a shell.
 */

import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'

import { requireShell } from '@/lib/shell'
import type {
  HostInfo,
  RuntimeEvent,
  RuntimeLogLine,
  RuntimePing,
  RuntimeSnapshot,
  RuntimeStatus,
} from './types'

/** Host -> frontend event names. Kept in one place so they cannot drift. */
export const RUNTIME_EVENT = {
  status: 'runtime://status',
  message: 'runtime://message',
  log: 'runtime://log',
} as const

/** Re-exported so consumers of this seam do not need a second import. */
export { isRunningInShell } from '@/lib/shell'

/** Current host-side runtime status. Never throws for a stopped runtime. */
export async function getRuntimeStatus(): Promise<RuntimeStatus> {
  requireShell()
  return invoke<RuntimeStatus>('runtime_status')
}

/** Ask the host to (re)start the runtime. Idempotent while it is already up. */
export async function startRuntime(): Promise<RuntimeStatus> {
  requireShell()
  return invoke<RuntimeStatus>('runtime_start')
}

/** Ask the host to stop the runtime and wait for the process to exit. */
export async function stopRuntime(): Promise<RuntimeStatus> {
  requireShell()
  return invoke<RuntimeStatus>('runtime_stop')
}

/** Stop then start. What the "reconnect" button calls. */
export async function restartRuntime(): Promise<RuntimeStatus> {
  requireShell()
  return invoke<RuntimeStatus>('runtime_restart')
}

/** Round-trip a `runtime.ping` through the bridge. */
export async function pingRuntime(): Promise<RuntimePing> {
  requireShell()
  return invoke<RuntimePing>('runtime_ping')
}

/** Recent stderr lines, oldest first. */
export async function getRuntimeLogs(limit = 200): Promise<RuntimeLogLine[]> {
  requireShell()
  return invoke<RuntimeLogLine[]>('runtime_logs', { limit })
}

export interface RuntimeEventHandlers {
  onStatus?: (status: RuntimeStatus) => void
  onEvent?: (event: RuntimeEvent) => void
  onLog?: (line: RuntimeLogLine) => void
}

export interface RuntimeSubscription {
  unsubscribe: () => void
}

/**
 * Subscribe to host pushes. Returns a handle; pass it to
 * {@link unsubscribeRuntimeEvents} when the consumer goes away.
 */
export async function subscribeRuntimeEvents(
  handlers: RuntimeEventHandlers,
): Promise<RuntimeSubscription> {
  requireShell()
  const unlisteners: UnlistenFn[] = await Promise.all([
    listen<RuntimeStatus>(RUNTIME_EVENT.status, (event) => handlers.onStatus?.(event.payload)),
    listen<RuntimeEvent>(RUNTIME_EVENT.message, (event) => handlers.onEvent?.(event.payload)),
    listen<RuntimeLogLine>(RUNTIME_EVENT.log, (event) => handlers.onLog?.(event.payload)),
  ])
  return {
    unsubscribe: () => {
      for (const unlisten of unlisteners) unlisten()
    },
  }
}

/** Release a subscription. Safe to call with `null` or twice. */
export function unsubscribeRuntimeEvents(subscription: RuntimeSubscription | null): void {
  subscription?.unsubscribe()
}

/** Re-exported so the store can type the snapshot without another import. */
export type { RuntimeSnapshot }

/** Facts about the shell: versions, platform, resolved workspace and binary. */
export async function getHostInfo(): Promise<HostInfo> {
  requireShell()
  return invoke<HostInfo>('runtime_host_info')
}
