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

/**
 * Where a turn got to.
 *
 * A failed turn keeps the reason it failed; a cancelled one was stopped at the
 * user's request, which is not a failure and has nothing to explain.
 */
export type ChatStatus = 'pending' | 'done' | 'failed' | 'cancelled'

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
  status: 'running' | 'done' | 'failed' | 'cancelled'
  error?: RuntimeError | null
  /** The turn's `chat.*` events, in the order they were sent. */
  events?: ChatEvent[]
}

/** What `chat.history` answers with: the turns so far, oldest first. */
export interface ChatHistory {
  turns?: ChatHistoryTurn[]
}

/** One conversation in the runtime's workspace, without its turns. */
export interface ChatSessionSummary {
  id: string
  /** Empty until the conversation has a first prompt to be named after. */
  title: string
  created_at: string
  updated_at: string
  /** How many messages the model has seen. */
  messages: number
}

/** What `sessions.list` answers with: the conversations, and the current one. */
export interface ChatSessions {
  current?: string
  sessions?: ChatSessionSummary[]
}

/** What `sessions.new` and `sessions.open` answer with. */
export interface ChatSessionOpened {
  session: string
  turns: number
}

/**
 * What `sessions.rename` answers with: the conversation, and the name it now has.
 */
export interface ChatSessionRenamed {
  session: string
  title: string
}

/**
 * What `sessions.delete` answers with.
 *
 * `current` is the conversation the runtime is continuing now, which may be a
 * different one: deleting the conversation in hand moves the runtime on rather
 * than leaving it with nowhere to put the next prompt.
 */
export interface ChatSessionDeleted {
  deleted: string
  current: string
}

/** What `chat.cancel` answers with: whether there was a turn to stop. */
export interface ChatCancelResult {
  cancelled: boolean
}
