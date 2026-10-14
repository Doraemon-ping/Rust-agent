import { defineStore } from 'pinia'
import { computed, ref } from 'vue'

import {
  getHostInfo,
  getRuntimeLogs,
  getRuntimeStatus,
  isRunningInShell,
  pingRuntime,
  restartRuntime,
  startRuntime,
  stopRuntime,
  subscribeRuntimeEvents,
  unsubscribeRuntimeEvents,
} from '@/lib/runtime'
import type { RuntimeSubscription } from '@/lib/runtime'
import { toRuntimeError } from '@/lib/runtime/errors'
import type {
  HostInfo,
  RuntimeError,
  RuntimeEvent,
  RuntimeLogLine,
  RuntimePing,
  RuntimeSnapshot,
  RuntimeState,
  RuntimeStatus,
} from '@/lib/runtime/types'

/** Ring sizes for the two append-only feeds. */
const MAX_LOGS = 500
const MAX_EVENTS = 100

function nowIso(): string {
  return new Date().toISOString()
}

function blankStatus(): RuntimeStatus {
  return {
    state: 'stopped',
    pid: null,
    bridgePath: null,
    protocolVersion: null,
    startedAt: null,
    changedAt: nowIso(),
    lastError: null,
    lastPing: null,
    snapshot: null,
  }
}

/**
 * The single source of truth for runtime state in the frontend.
 *
 * Every value here comes from the host: either a command result or a pushed
 * event. Nothing is polled on a timer and nothing is inferred, so the UI shows
 * what the process actually reports.
 */
export const useRuntimeStore = defineStore('runtime', () => {
  const shell = isRunningInShell()
  const status = ref<RuntimeStatus>(blankStatus())
  const logs = ref<RuntimeLogLine[]>([])
  const events = ref<RuntimeEvent[]>([])
  const busy = ref(false)
  const connected = ref(false)
  const hostInfo = ref<HostInfo | null>(null)
  let subscription: RuntimeSubscription | null = null

  const state = computed<RuntimeState>(() => status.value.state)
  const snapshot = computed<RuntimeSnapshot | null>(() => status.value.snapshot)
  const lastError = computed<RuntimeError | null>(() => status.value.lastError)
  const ready = computed(() => status.value.state === 'ready')
  const stopped = computed(() => status.value.state === 'stopped')
  const occupied = computed(() =>
    ['starting', 'ready', 'stopping'].includes(status.value.state),
  )
  const recentErrors = computed(() =>
    logs.value.filter((line) => line.level === 'error').slice(-20),
  )

  function applyStatus(next: RuntimeStatus): void {
    status.value = next
  }

  function pushLog(line: RuntimeLogLine): void {
    logs.value = [...logs.value, line].slice(-MAX_LOGS)
  }

  function pushEvent(event: RuntimeEvent): void {
    events.value = [...events.value, event].slice(-MAX_EVENTS)
  }

  function fail(error: unknown, context: string): RuntimeError {
    const normalized = toRuntimeError(error)
    status.value = {
      ...status.value,
      state: 'error',
      changedAt: nowIso(),
      lastError: normalized,
    }
    pushLog({
      ts: nowIso(),
      level: 'error',
      message: `${context}: ${normalized.code}: ${normalized.message}`,
    })
    return normalized
  }

  /** Run a host command, folding its result into the status. */
  async function guarded(
    context: string,
    action: () => Promise<RuntimeStatus>,
  ): Promise<void> {
    if (busy.value) return
    busy.value = true
    try {
      applyStatus(await action())
    } catch (error) {
      fail(error, context)
    } finally {
      busy.value = false
    }
  }

  function guardInteractivity(): string | null {
    if (!shell) {
      return 'The desktop shell is not running this page, so the runtime cannot be controlled.'
    }
    return null
  }

  /** Subscribe to host pushes and load the current state. Idempotent. */
  async function connect(): Promise<void> {
    const blocked = guardInteractivity()
    if (blocked) {
      pushLog({ ts: nowIso(), level: 'error', message: blocked })
      return
    }
    if (subscription) return
    try {
      subscription = await subscribeRuntimeEvents({
        onStatus: applyStatus,
        onEvent: pushEvent,
        onLog: pushLog,
      })
      connected.value = true
    } catch (error) {
      fail(error, 'subscribe')
      return
    }
    await refresh()
  }

  /** Reload status and logs from the host. */
  async function refresh(): Promise<void> {
    if (!shell) return
    try {
      applyStatus(await getRuntimeStatus())
      logs.value = await getRuntimeLogs()
      hostInfo.value = await getHostInfo()
    } catch (error) {
      fail(error, 'refresh')
    }
  }

  async function start(): Promise<void> {
    await guarded('start', startRuntime)
  }

  async function stop(): Promise<void> {
    await guarded('stop', stopRuntime)
  }

  async function restart(): Promise<void> {
    await guarded('restart', restartRuntime)
  }

  async function ping(): Promise<RuntimePing | null> {
    if (!shell) return null
    busy.value = true
    try {
      const ping = await pingRuntime()
      // The host records the ping in its own status and pushes an event, but
      // folding the returned value in here keeps the UI honest even when the
      // event stream is not available.
      status.value = { ...status.value, lastPing: ping }
      return ping
    } catch (error) {
      fail(error, 'ping')
      return null
    } finally {
      busy.value = false
    }
  }

  function disconnect(): void {
    unsubscribeRuntimeEvents(subscription)
    subscription = null
    connected.value = false
  }

  return {
    shell,
    status,
    logs,
    events,
    busy,
    connected,
    hostInfo,
    state,
    snapshot,
    lastError,
    ready,
    stopped,
    occupied,
    recentErrors,
    connect,
    disconnect,
    refresh,
    start,
    stop,
    restart,
    ping,
  }
})
