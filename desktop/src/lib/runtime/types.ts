/**
 * Types shared by the runtime service, the Pinia store, and the views.
 *
 * Two shapes live here and it is worth keeping them apart:
 *
 * - `RuntimeStatus` and friends are *host* payloads. The Tauri backend owns
 *   them, so they use camelCase like any other JS object.
 * - `RuntimeSnapshot` is the *runtime's* own `runtime.status` result, passed
 *   through untouched. It keeps the bridge's snake_case wire format on
 *   purpose: translating it here would mean two spellings of the same fact.
 */

/** Lifecycle of the runtime child process, as tracked by the host. */
export type RuntimeState = 'starting' | 'ready' | 'stopping' | 'stopped' | 'error'

export interface RuntimeError {
  code: string
  message: string
}

/** Result of the bridge's `runtime.status`, verbatim. */
export interface RuntimeSnapshot {
  protocol_version: number
  bridge_version: string
  pid: number
  started_at: string
  uptime_ms: number
  workspace: string
  methods: string[]
  model: string | null
  config_error: string | null
  skills: { count: number; names: string[] }
  tasks: { total: number; pending: number; in_progress: number; completed: number }
  memory: { records: number; by_kind: Record<string, number> }
  cron: { jobs: number; pending_delivery: number }
  plugins: { available: number; connected: number; names: string[] }
  workflow: { last_run: string | null }
  paths: Record<string, string>
  warnings: string[]
}

export interface RuntimePing {
  ok: boolean
  latencyMs: number
  at: string
  uptimeMs: number
  /** Why the round trip failed, when it did. */
  error: RuntimeError | null
}

/** Everything the UI needs to render the runtime, in one payload. */
export interface RuntimeStatus {
  state: RuntimeState
  pid: number | null
  bridgePath: string | null
  protocolVersion: number | null
  startedAt: string | null
  /** When `state` last changed; drives "last change" in the UI. */
  changedAt: string
  lastError: RuntimeError | null
  lastPing: RuntimePing | null
  snapshot: RuntimeSnapshot | null
}

export type LogLevel = 'info' | 'warn' | 'error' | 'raw'

/** One line of runtime stderr, or a host note about the process. */
export interface RuntimeLogLine {
  ts: string
  level: LogLevel
  message: string
}

/** An asynchronous event pushed by the bridge (never a request response). */
export interface RuntimeEvent {
  event: string
  ts: string
  data: unknown
}

/** Command surface the host exposes over Tauri IPC. */
export interface RuntimeCommands {
  runtime_status: () => Promise<RuntimeStatus>
  runtime_start: () => Promise<RuntimeStatus>
  runtime_stop: () => Promise<RuntimeStatus>
  runtime_restart: () => Promise<RuntimeStatus>
  runtime_ping: () => Promise<RuntimePing>
  runtime_logs: (limit?: number) => Promise<RuntimeLogLine[]>
  runtime_host_info: () => Promise<HostInfo>
  chat_send: (prompt: string) => Promise<{ accepted: boolean; turn: string }>
  chat_cancel: () => Promise<{ cancelled: boolean }>
  chat_history: () => Promise<{ turns: unknown[] }>
  chat_context: () => Promise<{
    session: string
    messages: number
    compacted: number
    chars: number
    char_limit: number | null
  }>
  chat_sessions: () => Promise<{ current?: string; sessions?: unknown[] }>
  chat_new_session: () => Promise<{ session: string; turns: number }>
  chat_open_session: (id: string) => Promise<{ session: string; turns: number }>
  chat_rename_session: (id: string, title: string) => Promise<{ session: string; title: string }>
  chat_delete_session: (id: string) => Promise<{ deleted: string; current: string }>
  cron_list: () => Promise<{ jobs: unknown[]; pending_delivery: number }>
  cron_schedule: (input: {
    cron: string
    prompt: string
    recurring: boolean
    durable: boolean
  }) => Promise<{ job: unknown }>
  cron_cancel: (id: string) => Promise<{ cancelled: string }>
  skills_list: () => Promise<{ dir: string; count: number; skills: unknown[] }>
  skills_read: (name: string) => Promise<unknown>
  skills_import: () => Promise<{
    dir: string
    count: number
    skills: unknown[]
  } | null>
  plugins_list: () => Promise<{ available: number; connected: string[]; servers: unknown[] }>
  plugins_connect: (server: string) => Promise<{ server: string; message: string }>
  departments_list: () => Promise<unknown>
  departments_create: (input: {
    name: string
    description: string
    instructions: string
    parentId: string | null
  }) => Promise<unknown>
  departments_remove: (id: string) => Promise<unknown>
  agents_add: (input: {
    departmentId: string
    name: string
    role: string
    instructions: string
  }) => Promise<unknown>
  agents_remove: (input: { departmentId: string; agentId: string }) => Promise<unknown>
  departments_run: (input: { id: string; instruction: string }) => Promise<unknown>
  departments_stop: (id: string) => Promise<unknown>
}

export const RUNTIME_STATE_LABEL: Record<RuntimeState, string> = {
  starting: 'Starting',
  ready: 'Ready',
  stopping: 'Stopping',
  stopped: 'Stopped',
  error: 'Error',
}

/**
 * One tone per lifecycle state, so every surface that tints it agrees.
 *
 * `neutral` is the honest default: `stopped` is not a problem.
 */
export function runtimeStateTone(state: RuntimeState): 'ok' | 'warn' | 'danger' | 'neutral' {
  switch (state) {
    case 'ready':
      return 'ok'
    case 'error':
      return 'danger'
    case 'starting':
    case 'stopping':
      return 'warn'
    default:
      return 'neutral'
  }
}

/** Facts about the desktop shell itself, not the runtime. */
export interface HostInfo {
  appVersion: string
  tauriVersion: string
  os: string
  arch: string
  /** Protocol revision this host build speaks. */
  protocolVersion: number
  /** Workspace the host will hand the runtime. */
  workspace: string
  /** Where the host resolves the bridge binary, for diagnostics. */
  bridgeCandidates: string[]
}
