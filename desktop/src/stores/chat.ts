import { defineStore } from 'pinia'
import { computed, ref, watch } from 'vue'

import { fetchChatHistory, sendChatPrompt, subscribeChatEvents } from '@/lib/chat'
import type { ChatEvent, ChatEventData, ChatToolBlock, ChatTurn } from '@/lib/chat'
import { toRuntimeError } from '@/lib/runtime/errors'
import type { RuntimeError } from '@/lib/runtime/types'
import { useRuntimeStore } from '@/stores/runtime'

/** How many turns stay on screen, and how much of one tool result is kept. */
const MAX_TURNS = 50
const MAX_OUTPUT = 4000

/**
 * How many events may wait for the turn they name. Only a page that reads the
 * conversation back while a turn is still being written can make one, so this
 * bounds a runaway rather than queueing ordinary work.
 */
const MAX_DEFERRED = 200

/**
 * The conversation, as the runtime reports it.
 *
 * Nothing here is invented. A prompt is added when the runtime accepts it, and
 * the transcript grows only from the events the runtime sends, so what the page
 * shows is what the loop actually did -- including the tool calls it made and
 * the calls its own permission policy refused.
 */
export const useChatStore = defineStore('chat', () => {
  const runtime = useRuntimeStore()
  const turns = ref<ChatTurn[]>([])
  const error = ref<RuntimeError | null>(null)
  const connected = ref(false)
  let unsubscribe: (() => void) | null = null
  let sequence = 0
  /** Events that arrived before the turn they name was known. */
  let deferred: ChatEvent[] = []

  /** Whether the running runtime offers a prompt method at all. */
  const supported = computed(() => (runtime.snapshot?.methods ?? []).includes('chat.send'))
  /** Whether it can also hand the conversation back when the page asks. */
  const historySupported = computed(() =>
    (runtime.snapshot?.methods ?? []).includes('chat.history'),
  )
  /** A turn is in flight; the runtime takes one prompt at a time. */
  const pending = computed(() => turns.value.some((turn) => turn.status === 'pending'))
  /** Whether the composer may send. */
  const canSend = computed(() => runtime.ready && supported.value && !pending.value)

  /**
   * The turn an event belongs to.
   *
   * A delta can reach the page before the accept that names its turn, so an
   * event for an id we do not know yet belongs to the one turn that is waiting.
   */
  function turnFor(id: string | undefined): ChatTurn | null {
    if (id) {
      const named = turns.value.find((turn) => turn.id === id)
      if (named) return named
    }
    return turns.value.find((turn) => turn.status === 'pending') ?? null
  }

  /** Extend the answer that is already being written, or start a new one. */
  function appendText(turn: ChatTurn, text: string): void {
    if (text === '') return
    const last = turn.blocks.at(-1)
    if (last?.kind === 'text') last.text += text
    else turn.blocks.push({ kind: 'text', text })
  }

  /** Fold one runtime event into the transcript. */
  function apply(event: ChatEvent): void {
    const data: ChatEventData = event.data ?? {}
    const turn = turnFor(data.turn)
    if (!turn) {
      // Nothing to fold it into yet: hold it until the conversation has been
      // read back, rather than dropping a piece of a turn that is still being
      // written.
      deferred.push(event)
      if (deferred.length > MAX_DEFERRED) deferred.shift()
      return
    }

    switch (event.event) {
      case 'chat.delta':
        appendText(turn, data.text ?? '')
        break

      case 'chat.tool_call':
        turn.blocks.push({
          kind: 'tool',
          name: data.name ?? 'tool',
          source: data.source ?? 'main',
          input: data.input ?? '',
          output: null,
          isError: false,
        })
        break

      case 'chat.tool_result': {
        // The result names its tool but not its call, so it lands on the last
        // call of that name that is still running.
        const running = turn.blocks
          .filter((block): block is ChatToolBlock => block.kind === 'tool' && block.output === null)
          .filter((block) => data.name === undefined || block.name === data.name)
          .at(-1)
        const isError = Boolean(data.is_error)
        if (running) {
          running.output = clamp(data.output ?? '')
          running.isError = isError
        } else {
          turn.blocks.push({
            kind: 'tool',
            name: data.name ?? 'tool',
            source: data.source ?? 'main',
            input: '',
            output: clamp(data.output ?? ''),
            isError,
          })
        }
        break
      }

      case 'chat.blocked':
        turn.blocks.push({
          kind: 'blocked',
          name: data.name ?? 'tool',
          reason: data.reason ?? '没有说明原因',
        })
        break

      case 'chat.notice':
        if ((data.text ?? '') !== '') turn.blocks.push({ kind: 'notice', text: data.text ?? '' })
        break

      case 'chat.done':
        // The final text is authoritative, but the deltas already said it: it is
        // only filled in when nothing streamed at all.
        if (!turn.blocks.some((block) => block.kind === 'text' && block.text.trim() !== '')) {
          appendText(turn, data.text ?? '')
        }
        turn.status = 'done'
        turn.error = null
        break

      case 'chat.failed':
        turn.status = 'failed'
        turn.error = data.error ?? {
          code: 'chat_failed',
          message: 'Runtime 报告这一轮失败，但没有给出原因。',
        }
        break

      default:
        break
    }
  }

  /** Subscribe to the runtime's turn events. Idempotent. */
  async function connect(): Promise<void> {
    if (!runtime.shell) {
      error.value = {
        code: 'not_in_shell',
        message: '当前页面不在桌面宿主中，收不到对话事件。请用 `pnpm app:dev` 启动桌面外壳。',
      }
      return
    }
    if (unsubscribe) return
    try {
      unsubscribe = await subscribeChatEvents(apply)
      connected.value = true
      error.value = null
    } catch (failure) {
      error.value = toRuntimeError(failure)
      return
    }
    // This window may be a reload of one that was already in conversation.
    await restore()
  }

  /** The re-read that is in flight, if any. */
  let restoring: Promise<void> | null = null

  /**
   * Read the conversation back from the runtime.
   *
   * Reloading the window makes a new page, not a new conversation: the runtime
   * never stopped holding the turns, so the page reads them back and replays
   * each through the folding the live events go through. A runtime that cannot
   * answer is left alone, rather than emptying a transcript that may be right.
   *
   * A reload asks twice -- once as the page connects, once as the status says
   * the runtime is up -- and one at a time is enough: a second answer would
   * replace a transcript the first is already extending with live events.
   */
  function restore(): Promise<void> {
    if (!runtime.ready || !historySupported.value) return Promise.resolve()
    restoring ??= readHistory().finally(() => {
      restoring = null
    })
    return restoring
  }

  async function readHistory(): Promise<void> {
    let history
    try {
      history = await fetchChatHistory()
    } catch (failure) {
      error.value = toRuntimeError(failure)
      return
    }

    const kept = (history.turns ?? []).slice(-MAX_TURNS)
    turns.value = kept.map<ChatTurn>((turn) => ({
      id: turn.turn,
      localId: `history-${turn.turn}`,
      prompt: turn.prompt,
      blocks: [],
      status: turn.status === 'running' ? 'pending' : turn.status,
      error: turn.error ?? null,
    }))
    for (const turn of kept) {
      for (const recorded of turn.events ?? []) apply(recorded)
    }

    const waiting = deferred
    deferred = []
    for (const event of waiting) apply(event)
  }

  function disconnect(): void {
    unsubscribe?.()
    unsubscribe = null
    connected.value = false
  }

  /** Send one prompt. `false` means it never left the page. */
  async function send(prompt: string): Promise<boolean> {
    const text = prompt.trim()
    if (text === '' || !canSend.value) return false
    error.value = null

    sequence += 1
    turns.value.push({
      id: '',
      localId: `turn-${sequence}`,
      prompt: text,
      blocks: [],
      status: 'pending',
      error: null,
    })
    while (turns.value.length > MAX_TURNS) turns.value.shift()
    const turn = turns.value.at(-1)
    if (!turn) return false

    try {
      turn.id = (await sendChatPrompt(text)).turn
      return true
    } catch (failure) {
      turn.status = 'failed'
      turn.error = toRuntimeError(failure)
      return false
    }
  }

  /** Drop the transcript. The runtime keeps its own history. */
  function clear(): void {
    turns.value = []
  }

  // A turn cannot outlive the process running it: when the runtime leaves
  // `ready` with a turn in flight, that answer is gone. Saying so beats a
  // spinner that never stops.
  watch(
    () => runtime.state,
    (state) => {
      if (state === 'ready') {
        // Whatever this runtime holds is the conversation now. A restart may
        // have moved to another project, so the page re-reads instead of
        // keeping a transcript of a runtime that is gone.
        void restore()
        return
      }
      for (const turn of turns.value) {
        if (turn.status !== 'pending') continue
        turn.status = 'failed'
        turn.error = {
          code: 'runtime_unavailable',
          message: `Runtime 状态变为 ${state}，这一轮没有完成。`,
        }
      }
      // A turn cannot outlive the process running it, and neither can an event
      // that was waiting for one: the next runtime numbers its turns from one
      // again, so a leftover would land in a stranger's turn.
      deferred = []
    },
  )

  return {
    turns,
    error,
    connected,
    supported,
    pending,
    canSend,
    connect,
    disconnect,
    send,
    clear,
  }
})

/** Keep one tool result from filling the transcript with a build log. */
function clamp(text: string): string {
  if (text.length <= MAX_OUTPUT) return text
  return `${text.slice(0, MAX_OUTPUT)}\n... 已截断 ${text.length - MAX_OUTPUT} 个字符`
}
