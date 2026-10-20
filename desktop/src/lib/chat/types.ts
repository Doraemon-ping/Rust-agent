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
  | {
      kind: 'notice'
      text: string
      /** The runtime's own kind for it, e.g. `compacted`, when it sent one. */
      notice?: string
    }
  | {
      kind: 'permission'
      /** The runtime's name for this request, which a decision has to name back. */
      id: string
      /** The tool whose call is waiting. */
      name: string
      /** That call's arguments, as the runtime rendered them. */
      input: string
      /** Why the policy flagged it, in the runtime's own words. */
      reason: string
      source: string
      /** `pending` until the decision is reported, which is the only truth. */
      decision: 'pending' | 'allowed' | 'denied'
    }
  | {
      kind: 'question'
      /** The runtime's name for this question, which an answer has to name back. */
      id: string
      question: string
      /** Suggested answers; the user may still answer in their own words. */
      options: string[]
      source: string
      /** `null` until somebody answers, and for a question nobody did. */
      answer: string | null
      /** Whether it is over: answered, or given up on. */
      resolved: boolean
    }

/** A tool call, once the template has narrowed a block down to one. */
export type ChatToolBlock = Extract<ChatBlock, { kind: 'tool' }>

/** A permission request, once the template has narrowed a block down to one. */
export type ChatPermissionBlock = Extract<ChatBlock, { kind: 'permission' }>

/** A question, once the template has narrowed a block down to one. */
export type ChatQuestionBlock = Extract<ChatBlock, { kind: 'question' }>

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
  /** The interaction a `chat.permission*` or `chat.question*` event is about. */
  id?: string
  question?: string
  options?: string[]
  answer?: string | null
  allowed?: boolean
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

/**
 * What `chat.answer` and `chat.approve` answer with.
 *
 * `accepted` is whether the answer reached the interaction it named: a page
 * that answered a question that was already over is told so rather than left to
 * believe it was heard.
 */
export interface ChatInteractionResult {
  accepted: boolean
}

/**
 * What `chat.context` answers with: the size of what the loop is holding.
 *
 * `chars` is the estimate the runtime's own compaction pipeline measures before
 * every model call, `char_limit` is the threshold it measures against, and
 * `compacted` is how many times it has already shrunk the conversation on its
 * own. All three come from the runtime: the page shows them, it does not guess.
 */
export interface ChatContext {
  /** The conversation these numbers describe. */
  session: string
  /** Messages the model has seen. */
  messages: number
  /** How many times the runtime has compacted the history. */
  compacted: number
  /** Estimated size of the history, in characters of JSON. */
  chars: number
  /** The size above which the runtime compacts, when it knows it. */
  char_limit: number | null
}
