/**
 * The chat, as the page sees it.
 *
 * One of the seam modules -- alongside `lib/runtime`, `lib/projects`, and
 * `lib/provider` -- which are the only files that import `@tauri-apps/api`. A
 * prompt is the one thing the page may choose here: the workspace, the model,
 * and the credentials are all decided in the host.
 */

import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'

import { RUNTIME_EVENT } from '@/lib/runtime'
import { requireShell } from '@/lib/shell'
import type {
  ChatAccepted,
  ChatCancelResult,
  ChatEvent,
  ChatHistory,
  ChatSessionOpened,
  ChatSessions,
} from './types'

/**
 * Accept one prompt.
 *
 * The answer does not come back here: the runtime reports the turn as
 * `chat.*` events, and this resolves as soon as it has taken the prompt.
 */
export async function sendChatPrompt(prompt: string): Promise<ChatAccepted> {
  requireShell()
  return invoke<ChatAccepted>('chat_send', { prompt })
}

/**
 * The conversation the runtime is holding.
 *
 * A reloaded page is a new page but not a new conversation: the runtime keeps
 * the turns, so the page reads them back instead of starting blank.
 */
export async function fetchChatHistory(): Promise<ChatHistory> {
  requireShell()
  return invoke<ChatHistory>('chat_history')
}

/**
 * Stop the turn that is running.
 *
 * The runtime answers whether there was one to stop: the page asked, and the
 * page is told what happened rather than what it hoped for.
 */
export async function cancelChatTurn(): Promise<ChatCancelResult> {
  requireShell()
  return invoke<ChatCancelResult>('chat_cancel')
}

/** The conversations the runtime holds for this workspace. */
export async function fetchChatSessions(): Promise<ChatSessions> {
  requireShell()
  return invoke<ChatSessions>('chat_sessions')
}

/** Start a fresh conversation. The old one stays on disk. */
export async function newChatSession(): Promise<ChatSessionOpened> {
  requireShell()
  return invoke<ChatSessionOpened>('chat_new_session')
}

/** Continue one the workspace already holds. */
export async function openChatSession(id: string): Promise<ChatSessionOpened> {
  requireShell()
  return invoke<ChatSessionOpened>('chat_open_session', { id })
}

/** Subscribe to the runtime's turn events. Returns the way to stop listening. */
export async function subscribeChatEvents(
  onEvent: (event: ChatEvent) => void,
): Promise<() => void> {
  requireShell()
  const unlisten: UnlistenFn = await listen<ChatEvent>(RUNTIME_EVENT.chat, (event) =>
    onEvent(event.payload),
  )
  return unlisten
}

export type {
  ChatAccepted,
  ChatBlock,
  ChatCancelResult,
  ChatEvent,
  ChatEventData,
  ChatHistory,
  ChatHistoryTurn,
  ChatSessionOpened,
  ChatSessionSummary,
  ChatSessions,
  ChatStatus,
  ChatToolBlock,
  ChatTurn,
} from './types'
