import { defineStore } from 'pinia'
import { computed, ref, watch } from 'vue'

import {
  answerChatQuestion,
  approveChatCall,
  cancelChatTurn,
  deleteChatSession,
  fetchChatContext,
  fetchChatHistory,
  fetchChatSessions,
  newChatSession,
  openChatSession,
  renameChatSession,
  sendChatPrompt,
  subscribeChatEvents,
} from '@/lib/chat'
import type {
  ChatEvent,
  ChatEventData,
  ChatContext,
  ChatPermissionBlock,
  ChatQuestionBlock,
  ChatSessionOpened,
  ChatSessionSummary,
  ChatToolBlock,
  ChatTurn,
} from '@/lib/chat'
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
  /** The conversations this workspace holds, newest first. */
  const sessions = ref<ChatSessionSummary[]>([])
  /** Which of them is being continued. */
  const current = ref('')
  /** Why the conversation list -- or the last change to it -- failed. */
  const sessionError = ref<RuntimeError | null>(null)
  /**
   * What the loop is holding, as the runtime reports it.
   *
   * Kept apart from `error`: this is a readout, and a readout that could not be
   * read leaves the badge off rather than putting a banner over a conversation
   * that is working.
   */
  const context = ref<ChatContext | null>(null)
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
  /** Whether it can stop the turn that is running. */
  const cancelSupported = computed(() => (runtime.snapshot?.methods ?? []).includes('chat.cancel'))
  /** Whether it can say which conversations it holds. */
  const sessionsSupported = computed(() =>
    (runtime.snapshot?.methods ?? []).includes('sessions.list'),
  )
  /** Whether it can say how much of the conversation it is holding. */
  const contextSupported = computed(() =>
    (runtime.snapshot?.methods ?? []).includes('chat.context'),
  )
  /** Whether the runtime can take an answer, or a decision, from the page. */
  const interactionSupported = computed(() =>
    (runtime.snapshot?.methods ?? []).includes('chat.answer'),
  )
  /**
   * What the runtime is waiting on a person for, if anything.
   *
   * Read back out of the transcript rather than remembered here: an interaction
   * that has been answered, or dropped by a turn that ended, is not waiting any
   * more, and only the runtime's own events say so.
   */
  const waiting = computed<ChatPermissionBlock | ChatQuestionBlock | null>(() => {
    for (const turn of turns.value) {
      for (const block of turn.blocks) {
        if (block.kind === 'permission' && block.decision === 'pending') return block
        if (block.kind === 'question' && !block.resolved) return block
      }
    }
    return null
  })

  /** Whether the runtime has compacted the conversation at all. */
  const compacted = computed(() => context.value?.compacted ?? 0)
  /** A turn is in flight; the runtime takes one prompt at a time. */
  const pending = computed(() => turns.value.some((turn) => turn.status === 'pending'))
  /** Whether the composer may send. */
  const canSend = computed(() => runtime.ready && supported.value && !pending.value)
  /** Whether the stop button is worth offering right now. */
  const canCancel = computed(() => runtime.ready && cancelSupported.value && pending.value)
  /** Whether another conversation may be chosen: not mid-turn. */
  const canSwitchSessions = computed(() => runtime.ready && sessionsSupported.value && !pending.value)
  /** The name of the conversation being continued, when it has one. */
  const currentTitle = computed(
    () => sessions.value.find((session) => session.id === current.value)?.title ?? '',
  )

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

  /**
   * Close whatever a turn was still waiting on.
   *
   * A turn that is over leaves no question open: the runtime either answered it
   * or it is gone. A transcript that kept one pending would be waiting forever
   * on a person nobody is asking any more.
   */
  function closeInteractions(turn: ChatTurn): void {
    for (const block of turn.blocks) {
      if (block.kind === 'permission' && block.decision === 'pending') block.decision = 'denied'
      if (block.kind === 'question' && !block.resolved) block.resolved = true
    }
  }

  /** The permission request an event names, if the transcript still holds it. */
  function permissionBlock(turn: ChatTurn, id: string | undefined): ChatPermissionBlock | null {
    return (
      turn.blocks
        .filter((block): block is ChatPermissionBlock => block.kind === 'permission')
        .filter((block) => id === undefined || block.id === id)
        .at(-1) ?? null
    )
  }

  /** The question an event names, if the transcript still holds it. */
  function questionBlock(turn: ChatTurn, id: string | undefined): ChatQuestionBlock | null {
    return (
      turn.blocks
        .filter((block): block is ChatQuestionBlock => block.kind === 'question')
        .filter((block) => id === undefined || block.id === id)
        .at(-1) ?? null
    )
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
        if ((data.text ?? '') !== '') {
          turn.blocks.push({
            kind: 'notice',
            text: data.text ?? '',
            // Kept so the transcript can tell a compaction apart from a
            // memory recall without reading the runtime's prose.
            notice: data.kind,
          })
        }
        break

      case 'chat.permission':
        turn.blocks.push({
          kind: 'permission',
          id: data.id ?? '',
          name: data.name ?? 'tool',
          input: data.input ?? '',
          reason: data.reason ?? '',
          source: data.source ?? 'main',
          decision: 'pending',
        })
        break

      case 'chat.permission_result': {
        // The decision names the request it belongs to: only the runtime says
        // whether a call may run, and it says so once.
        const waiting = permissionBlock(turn, data.id)
        if (waiting) waiting.decision = data.allowed ? 'allowed' : 'denied'
        break
      }

      case 'chat.question':
        turn.blocks.push({
          kind: 'question',
          id: data.id ?? '',
          question: data.question ?? '',
          options: data.options ?? [],
          source: data.source ?? 'main',
          answer: null,
          resolved: false,
        })
        break

      case 'chat.question_result': {
        const waiting = questionBlock(turn, data.id)
        if (waiting) {
          waiting.answer = data.answer ?? null
          waiting.resolved = true
        }
        break
      }

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
        closeInteractions(turn)
        break

      case 'chat.cancelled':
        // Stopped, not broken: the runtime rolled the attempt back, so there is
        // nothing to explain and nothing to keep in the conversation.
        turn.status = 'cancelled'
        turn.error = null
        closeInteractions(turn)
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
    await refreshSessions()
    await refreshContext()
  }

  /**
   * Read back how much the runtime is holding for this conversation.
   *
   * The runtime compacts a long history before every model call; the page asks
   * after each turn so the number a reader sees is the one the loop is
   * actually working with, not one the page remembered.
   */
  async function refreshContext(): Promise<void> {
    if (!runtime.ready || !contextSupported.value) return
    try {
      context.value = await fetchChatContext()
    } catch {
      // A readout nobody can read is simply not shown.
      context.value = null
    }
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

  /** Read the conversation back again, after the runtime switched to another. */
  async function readAgain(): Promise<void> {
    const inFlight = restoring
    if (inFlight) await inFlight
    restoring = null
    await restore()
  }

  /**
   * Which conversations this workspace holds.
   *
   * The runtime owns the list -- it is the one that writes the files -- so the
   * page asks rather than keeping its own idea of what was said when.
   */
  async function refreshSessions(): Promise<void> {
    if (!runtime.ready || !sessionsSupported.value) return
    try {
      const answer = await fetchChatSessions()
      sessions.value = answer.sessions ?? []
      current.value = answer.current ?? ''
      sessionError.value = null
    } catch (failure) {
      sessionError.value = toRuntimeError(failure)
    }
  }

  /**
   * Stop the turn that is running.
   *
   * The turn ends as `chat.cancelled`, not here: the runtime may be in the
   * middle of a tool call, and only it knows when the turn has actually
   * stopped. `false` means there was nothing to stop.
   */
  async function cancel(): Promise<boolean> {
    if (!canCancel.value) return false
    try {
      return (await cancelChatTurn()).cancelled
    } catch (failure) {
      error.value = toRuntimeError(failure)
      return false
    }
  }

  /**
   * Answer the question the runtime is waiting on.
   *
   * The runtime is the one holding the question, so it is the one that decides
   * whether the answer arrived. Nothing is written into the transcript here: a
   * page cannot show an answer that never reached the model.
   */
  async function answer(id: string, text: string): Promise<boolean> {
    if (!interactionSupported.value) return false
    try {
      return (await answerChatQuestion(id, text)).accepted
    } catch (failure) {
      error.value = toRuntimeError(failure)
      return false
    }
  }

  /**
   * Allow or refuse the call the runtime is waiting on.
   *
   * The decision reaches the call the id names, and the runtime reports what it
   * decided as `chat.permission_result` -- which is what the page shows.
   */
  async function decide(id: string, allow: boolean): Promise<boolean> {
    if (!interactionSupported.value) return false
    try {
      return (await approveChatCall(id, allow)).accepted
    } catch (failure) {
      error.value = toRuntimeError(failure)
      return false
    }
  }

  /** Switch conversations, then show the one the runtime switched to. */
  async function switchTo(load: () => Promise<ChatSessionOpened>): Promise<void> {
    if (!canSwitchSessions.value) return
    error.value = null
    try {
      const opened = await load()
      current.value = opened.session
      turns.value = []
      deferred = []
      await readAgain()
    } catch (failure) {
      error.value = toRuntimeError(failure)
    }
    await refreshSessions()
  }

  /** Start a fresh conversation. The one in hand stays on disk. */
  function startSession(): Promise<void> {
    return switchTo(newChatSession)
  }

  /** Continue a conversation this workspace already holds. */
  function openSession(id: string): Promise<void> {
    return switchTo(() => openChatSession(id))
  }

  /**
   * Give a conversation a name of its own.
   *
   * The page stays where it is: a name is not a turn, and the runtime keeps
   * continuing the same conversation. A conversation that has no file yet is
   * named anyway, so naming one before its first prompt works rather than being
   * thrown away when it is written.
   */
  async function renameSession(id: string, title: string): Promise<boolean> {
    if (!canSwitchSessions.value) return false
    sessionError.value = null
    try {
      await renameChatSession(id, title)
    } catch (failure) {
      sessionError.value = toRuntimeError(failure)
      return false
    }
    await refreshSessions()
    return true
  }

  /**
   * Forget a conversation.
   *
   * The one on screen can go too: the runtime continues whatever is left, or
   * starts an empty conversation, and this page reads that back instead of
   * keeping a transcript of something that is no longer there.
   */
  async function deleteSession(id: string): Promise<boolean> {
    if (!canSwitchSessions.value) return false
    sessionError.value = null
    try {
      const answer = await deleteChatSession(id)
      current.value = answer.current
      turns.value = []
      deferred = []
      await readAgain()
    } catch (failure) {
      sessionError.value = toRuntimeError(failure)
      return false
    }
    await refreshSessions()
    return true
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
        void refreshSessions()
        void refreshContext()
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
      context.value = null
    },
  )

  // A turn that just ended may have named the conversation it belongs to -- the
  // first prompt is what a conversation is called -- or changed how much it
  // holds, so the list is re-read once per turn rather than left stale.
  watch(pending, (running, wasRunning) => {
    if (wasRunning && !running) {
      void refreshSessions()
      // A turn is what grows the history, and what makes the runtime compact.
      void refreshContext()
    }
  })

  return {
    turns,
    error,
    connected,
    supported,
    pending,
    canSend,
    sessions,
    sessionError,
    context,
    compacted,
    current,
    currentTitle,
    cancelSupported,
    sessionsSupported,
    canCancel,
    canSwitchSessions,
    contextSupported,
    interactionSupported,
    waiting,
    refreshSessions,
    refreshContext,
    startSession,
    openSession,
    renameSession,
    deleteSession,
    cancel,
    connect,
    disconnect,
    send,
    clear,
    answer,
    decide,
  }
})

/** Keep one tool result from filling the transcript with a build log. */
function clamp(text: string): string {
  if (text.length <= MAX_OUTPUT) return text
  return `${text.slice(0, MAX_OUTPUT)}\n... 已截断 ${text.length - MAX_OUTPUT} 个字符`
}
