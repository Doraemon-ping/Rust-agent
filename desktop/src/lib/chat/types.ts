/**
 * The chat, as the page sees it.
 *
 * Two shapes live here and it is worth keeping them apart. `ChatTurn` is the
 * page's own: one prompt and everything the runtime said about it. `ChatEvent`
 * is the bridge's wire shape, passed through the host untouched, which is why
 * its fields keep the runtime's snake_case.
 */

import type { RuntimeError } from '@/lib/runtime/types'

/** One thing the runtime did during a turn, in the order it did it. */
export type ChatBlock =
  | { kind: 'text'; text: string }
  | {
      kind: 'tool'
      name: string
      /** `main` for the conversation's own loop, `subagent` for a `task` call. */
      source: string
      /** The call's arguments, as the runtime rendered them. */
      input: string
      /** `null` while the call is still running. */
      output: string | null
      isError: boolean
    }
  | { kind: 'blocked'; name: string; reason: string }
  | { kind: 'notice'; text: string }

/** A tool call, once the template has narrowed a block down to one. */
export type ChatToolBlock = Extract<ChatBlock, { kind: 'tool' }>

/** Where a turn got to. A failed turn keeps the reason it failed. */
export type ChatStatus = 'pending' | 'done' | 'failed'

export interface ChatTurn {
  /** The runtime's turn id, once the accept has named it; empty until then. */
  id: string
  /** Page-local and stable for the life of the turn: what the list keys on. */
  localId: string
  prompt: string
  blocks: ChatBlock[]
  status: ChatStatus
  error: RuntimeError | null
}

/** What the runtime puts in `data` on a `chat.*` event. */
export interface ChatEventData {
  turn?: string
  source?: string
  text?: string
  name?: string
  input?: string
  output?: string
  is_error?: boolean
  reason?: string
  kind?: string
  error?: RuntimeError
}

/** One `chat.*` event, as the host forwards it. */
export interface ChatEvent {
  event: string
  ts: string
  data?: ChatEventData
}

/** What `chat.send` answers with: accepted, and under which turn id. */
export interface ChatAccepted {
  accepted: boolean
  turn: string
}

/**
 * One turn as the runtime kept it, for a page reading the conversation back
 * rather than watching it arrive.
 *
 * The status is the runtime's own word, `running` included: a turn that is
 * still going has no terminal event yet to say otherwise.
 */
export interface ChatHistoryTurn {
  turn: string
  prompt: string
  status: 'running' | 'done' | 'failed'
  error?: RuntimeError | null
  /** The turn's `chat.*` events, in the order they were sent. */
  events?: ChatEvent[]
}

/** What `chat.history` answers with: the turns so far, oldest first. */
export interface ChatHistory {
  turns?: ChatHistoryTurn[]
}
