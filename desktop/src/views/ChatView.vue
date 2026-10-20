<script setup lang="ts">
import {
  Ban,
  Check,
  ChevronDown,
  Circle,
  CircleAlert,
  CircleCheckBig,
  History,
  LoaderCircle,
  MessageCircleQuestion,
  MessageSquare,
  Pencil,
  Plus,
  SendHorizontal,
  ShieldAlert,
  Shrink,
  Square,
  Trash2,
  Wrench,
  X,
} from '@lucide/vue'
import { computed, nextTick, ref, watch } from 'vue'
import { RouterLink } from 'vue-router'

import Alert from '@/components/ui/Alert.vue'
import Badge from '@/components/ui/Badge.vue'
import Button from '@/components/ui/Button.vue'
import type { ChatBlock, ChatToolBlock, ChatTurn } from '@/lib/chat'
import { formatRelative, shortenPath } from '@/lib/format'
import { RUNTIME_STATE_LABEL, runtimeStateTone } from '@/lib/runtime/types'
import { useChatStore } from '@/stores/chat'
import { useProjectsStore } from '@/stores/projects'
import { useRuntimeStore } from '@/stores/runtime'

const projects = useProjectsStore()
const runtime = useRuntimeStore()
const chat = useChatStore()

const draft = ref('')
const scroller = ref<HTMLElement | null>(null)
/** What has been typed into each question's own-words field, by interaction id. */
const answers = ref<Record<string, string>>({})

/** The session menu, so choosing from it closes it. */
const menu = ref<HTMLDetailsElement | null>(null)
/** The conversation the menu is renaming, and the name typed so far. */
const renaming = ref('')
const renameDraft = ref('')
/** The conversation the menu is asking about before it deletes it. */
const deleting = ref('')
/** Whether the reader is at the bottom; scrolling up must not be yanked back. */
let following = true

/** Longest conversation name the runtime takes, in characters. */
const MAX_TITLE_CHARS = 120

const active = computed(() => projects.active)
const stateTone = computed(() => runtimeStateTone(runtime.state))

/**
 * The project the runtime was started in.
 *
 * No project is not "no workspace": the host falls back to its own per-user
 * directory, so the page shows which one the prompt would run in.
 */
const workspace = computed(
  () => active.value?.path ?? runtime.hostInfo?.workspace ?? runtime.snapshot?.workspace ?? '--',
)

/** The one honest reason the page cannot send, when there is one. */
const issue = computed(() => {
  if (!runtime.shell) return '当前页面不在桌面宿主中：没有 Runtime 通道可对话。'
  if (runtime.state === 'error') {
    return runtime.lastError
      ? `${runtime.lastError.code}: ${runtime.lastError.message}`
      : 'Runtime 处于错误状态，详情见设置。'
  }
  if (runtime.ready && !chat.supported) {
    return '这个 Runtime 只上报自身状态，没有 chat.send 方法，因此收不下提示词。'
  }
  if (runtime.ready && runtime.snapshot?.config_error) {
    return `模型还没有配置好：${runtime.snapshot.config_error}`
  }
  if (!chat.connected && runtime.shell) {
    return chat.error ? `${chat.error.code}: ${chat.error.message}` : '还没有连上 Runtime 的事件通道。'
  }
  return null
})

const placeholder = computed(() => {
  if (!runtime.shell) return '桌面宿主没有运行这个页面'
  if (runtime.state === 'starting') return 'Runtime 正在启动…'
  if (!runtime.ready) return 'Runtime 未运行：到设置里启动它'
  if (!chat.supported) return '这个 Runtime 没有对话接口（需要 chat.send）'
  if (chat.waiting) return 'Runtime 正在等你回答上面的问题（也可以直接按停止）'
  if (chat.pending) return 'Runtime 正在回答上一个提示词…'
  return '给 Runtime 一个任务，例如「列出 src 下的 Rust 文件」'
})

const hint = computed(() => {
  if (chat.waiting) return 'Runtime 正停在这一轮里等你：回答上面的问题，或者批准它，它才会继续。'
  if (chat.pending) {
    return chat.canCancel
      ? '这一轮正在运行：可以停止它，或者等它结束；停下来的一轮不会进入上下文。'
      : 'Runtime 一次只处理一个提示词；上一轮结束后就能继续。'
  }
  if (!active.value) return '未选择项目：提示词会在上面这个宿主工作目录里执行。'
  return '回答、工具调用与权限拒绝都由 Runtime 上报，这一页只呈现，不编造。'
})

const canSubmit = computed(() => chat.canSend && draft.value.trim() !== '')

/**
 * What the runtime is holding for this conversation, in one line.
 *
 * The runtime compacts a long history before every model call, and nothing
 * about that reaches a reader unless it is said out loud. This says it: the
 * size it is holding against the threshold it compacts above, and how many
 * times it has already done so.
 */
const contextLine = computed(() => {
  const context = chat.context
  if (!context) return ''
  const size = `${compact(context.chars)} 字符`
  const limit = context.char_limit ? ` / ${compact(context.char_limit)}` : ''
  return `上下文 ${size}${limit} · 消息 ${context.messages} 条`
})

const contextTitle = computed(() => {
  const context = chat.context
  if (!context) return ''
  const limit = context.char_limit
    ? `超过 ${context.char_limit} 字符时 Runtime 会自己压缩`
    : '这个 Runtime 没有启用压缩'
  return `${contextLine.value}；已自动压缩 ${context.compacted} 次；${limit}`
})

/** Big numbers, shortened: the badge is a readout, not a ledger. */
function compact(value: number): string {
  if (value < 1000) return String(value)
  if (value < 1_000_000) return `${(value / 1000).toFixed(1)}k`
  return `${(value / 1_000_000).toFixed(1)}M`
}

/** The step name out of a compaction notice, e.g. `micro` from `(micro)`. */
function compactionStep(text: string): string {
  return text.match(/\(([^)]+)\)\s*$/)?.[1] ?? ''
}

/** A fingerprint of the transcript, so streaming text scrolls the view along. */
const size = computed(() =>
  chat.turns.reduce(
    (total, turn) => total + turn.blocks.reduce((sum, block) => sum + blockSize(block), 0),
    chat.turns.length,
  ),
)

function blockSize(block: ChatBlock): number {
  switch (block.kind) {
    case 'text':
      return block.text.length
    case 'tool':
      return (block.output ?? '').length + block.input.length + 1
    case 'permission':
      // A request is something the reader has to act on, not a line of prose.
      return block.reason.length + block.input.length + 1
    case 'question':
      return (
        block.question.length + block.options.join('').length + (block.answer?.length ?? 0) + 1
      )
    default:
      return 1
  }
}

function onScroll(): void {
  const element = scroller.value
  if (!element) return
  following = element.scrollHeight - element.scrollTop - element.clientHeight < 48
}

watch(size, async () => {
  if (!following) return
  await nextTick()
  const element = scroller.value
  if (element) element.scrollTop = element.scrollHeight
})

/**
 * A question is not progress to read at leisure: the turn is stopped until
 * somebody answers it. The view goes to it even if the reader had scrolled
 * away, and puts the cursor where the answer goes.
 */
watch(
  () => chat.waiting?.id ?? '',
  async (id) => {
    if (id === '') return
    following = true
    await nextTick()
    const element = scroller.value
    if (!element) return
    element.scrollTop = element.scrollHeight
    element.querySelector<HTMLInputElement>('input[data-answer]')?.focus()
  },
)

/** Allow or refuse the call the runtime is waiting on. */
async function decide(id: string, allow: boolean): Promise<void> {
  await chat.decide(id, allow)
}

/** Answer a question, in the user's own words or with one of the suggestions. */
async function answer(id: string, text: string): Promise<void> {
  const said = text.trim()
  if (said === '' || !(await chat.answer(id, said))) return
  // The runtime reports the answer back as an event; the draft is only thrown
  // away once it has been taken.
  delete answers.value[id]
}

/** What a decision the runtime reported means, in a word. */
function decisionLabel(decision: 'pending' | 'allowed' | 'denied'): string {
  if (decision === 'allowed') return '已允许'
  if (decision === 'denied') return '已拒绝'
  return '等待中'
}

/** The badge tone for that decision. */
function decisionTone(decision: 'pending' | 'allowed' | 'denied'): 'warn' | 'ok' | 'danger' {
  if (decision === 'allowed') return 'ok'
  if (decision === 'denied') return 'danger'
  return 'warn'
}

/** One step of a plan, as `todo_write` carries it. */
interface TodoItem {
  content: string
  status: string
}

/** How many steps of a plan are shown; the tool caps a list at twenty. */
const MAX_TODOS = 20

/** The icon that stands for a step's status. */
const TODO_ICON: Record<string, typeof Circle> = {
  pending: Circle,
  in_progress: LoaderCircle,
  completed: CircleCheckBig,
}

/**
 * The plan a `todo_write` call carries.
 *
 * The tool's arguments are the list itself, so the transcript can show a plan
 * as a plan instead of as JSON. A call the host cut short, or one that never
 * parsed, simply answers nothing and falls back to the raw arguments.
 */
function todos(block: ChatToolBlock): TodoItem[] {
  if (block.name !== 'todo_write') return []
  try {
    const parsed = JSON.parse(block.input) as { todos?: unknown }
    if (!Array.isArray(parsed.todos)) return []
    return parsed.todos
      .filter((step): step is TodoItem => {
        const candidate = step as TodoItem
        return typeof candidate?.content === 'string' && candidate.content.trim() !== ''
      })
      .slice(0, MAX_TODOS)
  } catch {
    return []
  }
}

/** The icon for a step's status. */
function todoIcon(status: string): typeof Circle {
  return TODO_ICON[status] ?? Circle
}

async function submit(): Promise<void> {
  const text = draft.value
  if (text.trim() === '' || !chat.canSend) return
  following = true
  if (await chat.send(text)) draft.value = ''
}

function suggest(text: string): void {
  draft.value = text
}

/** Run a session action and close the menu behind it. */
function pick(load: () => Promise<void>): void {
  menu.value?.removeAttribute('open')
  void load()
}

/** Open the name field on a conversation and put the cursor in it. */
function startRename(id: string, title: string): void {
  deleting.value = ''
  renaming.value = id
  renameDraft.value = title
}

/** Put the cursor in the name field the moment it appears. */
function focusRename(element: unknown): void {
  if (element instanceof HTMLInputElement) {
    element.focus()
    element.select()
  }
}

/**
 * Keep the name that was typed.
 *
 * An empty name closes the field instead of saving one: the list already has a
 * word for a conversation nobody named.
 */
async function commitRename(id: string): Promise<void> {
  const title = renameDraft.value.trim()
  if (title === '') {
    renaming.value = ''
    return
  }
  if (await chat.renameSession(id, title)) renaming.value = ''
}

/** Ask before deleting: a conversation is not something to lose by misclick. */
function askDelete(id: string): void {
  renaming.value = ''
  deleting.value = id
}

async function confirmDelete(id: string): Promise<void> {
  if (await chat.deleteSession(id)) deleting.value = ''
}

const SUGGESTIONS = [
  '这个项目是做什么的？看 README 和目录结构后告诉我。',
  '找出仓库里最近的 5 个 commit，用一句话概括每个。',
  '列出 src/ 下每个模块的职责，一行一个。',
]

function turnTone(turn: ChatTurn): string {
  if (turn.status === 'failed') return 'border-danger/35'
  if (turn.status === 'cancelled') return 'border-line/50'
  if (turn.status === 'pending') return 'border-line'
  return 'border-line/70'
}
</script>

<template>
  <div class="flex h-full min-h-0 flex-col">
    <header class="flex items-start justify-between gap-4 border-b border-line px-6 py-4">
      <div class="min-w-0">
        <p class="text-xs tracking-[0.18em] text-ink-faint uppercase">对话</p>
        <h1 class="mt-1 truncate text-lg font-semibold tracking-tight">
          {{ active ? active.name : '宿主工作目录' }}
        </h1>
        <p class="mt-0.5 truncate font-mono text-xs text-ink-faint" :title="workspace">
          {{ shortenPath(workspace, 72) }}
        </p>
      </div>
      <div class="flex shrink-0 items-center gap-2">
        <details v-if="chat.sessionsSupported" ref="menu" class="relative">
          <summary
            class="flex max-w-[16rem] cursor-pointer list-none items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-xs text-ink-muted hover:text-ink"
            title="这一页正在继续的会话；每一轮结束都会写进应用自己的存储目录，而不是项目目录"
          >
            <History class="size-3.5 shrink-0" />
            <span class="truncate">{{ chat.currentTitle || '未命名会话' }}</span>
            <ChevronDown class="size-3.5 shrink-0" />
          </summary>
          <div
            class="absolute right-0 z-20 mt-2 w-80 overflow-hidden rounded-lg border border-line bg-surface shadow-lg"
          >
            <button
              type="button"
              class="flex w-full items-center gap-2 px-3 py-2 text-left text-xs text-ink hover:bg-canvas disabled:opacity-50"
              :disabled="!chat.canSwitchSessions"
              @click="pick(chat.startSession)"
            >
              <Plus class="size-3.5 shrink-0 text-ink-faint" />
              新会话
            </button>
            <div v-if="chat.sessions.length" class="h-px bg-line/60" />
            <p v-if="!chat.sessions.length" class="px-3 py-2 text-xs text-ink-faint">
              还没有写过任何会话：第一条提示词之后，它会出现在这里。
            </p>
            <div
              v-for="session in chat.sessions"
              :key="session.id"
              class="flex items-center gap-1 px-2 py-1.5 hover:bg-canvas"
            >
              <template v-if="renaming === session.id">
                <input
                  :ref="focusRename"
                  v-model="renameDraft"
                  type="text"
                  class="min-w-0 flex-1 rounded border border-line bg-canvas px-2 py-1 text-xs text-ink outline-none"
                  :maxlength="MAX_TITLE_CHARS"
                  :placeholder="session.title || '未命名会话'"
                  @keydown.enter.prevent="commitRename(session.id)"
                  @keydown.esc.prevent="renaming = ''"
                />
                <button
                  type="button"
                  class="shrink-0 rounded p-1 text-ink-muted hover:text-ink"
                  title="保存名字"
                  @click="commitRename(session.id)"
                >
                  <Check class="size-3.5" />
                </button>
                <button
                  type="button"
                  class="shrink-0 rounded p-1 text-ink-muted hover:text-ink"
                  title="放弃这次改名"
                  @click="renaming = ''"
                >
                  <X class="size-3.5" />
                </button>
              </template>
              <template v-else-if="deleting === session.id">
                <span class="min-w-0 flex-1 truncate text-xs text-danger">
                  删除「{{ session.title || '未命名会话' }}」？
                </span>
                <button
                  type="button"
                  class="shrink-0 rounded px-2 py-1 text-xs text-danger hover:bg-danger/10 disabled:opacity-50"
                  :disabled="!chat.canSwitchSessions"
                  title="删掉这条会话，它的文件会一起消失"
                  @click="confirmDelete(session.id)"
                >
                  删除
                </button>
                <button
                  type="button"
                  class="shrink-0 rounded px-2 py-1 text-xs text-ink-muted hover:text-ink"
                  @click="deleting = ''"
                >
                  取消
                </button>
              </template>
              <template v-else>
                <button
                  type="button"
                  class="flex min-w-0 flex-1 flex-col gap-0.5 text-left hover:text-ink disabled:opacity-50"
                  :disabled="!chat.canSwitchSessions || session.id === chat.current"
                  :title="session.id"
                  @click="pick(() => chat.openSession(session.id))"
                >
                  <span class="flex items-center gap-2">
                    <span class="truncate text-xs text-ink-muted">
                      {{ session.title || '未命名会话' }}
                    </span>
                    <Badge v-if="session.id === chat.current" class="px-1.5 py-0 text-[10px]">
                      当前
                    </Badge>
                  </span>
                  <span class="text-[10px] text-ink-faint">
                    {{ session.messages }} 条消息 · {{ formatRelative(session.updated_at) }}
                  </span>
                </button>
                <button
                  type="button"
                  class="shrink-0 rounded p-1 text-ink-faint hover:text-ink disabled:opacity-30"
                  title="给这条会话改名"
                  :disabled="!chat.canSwitchSessions"
                  @click="startRename(session.id, session.title)"
                >
                  <Pencil class="size-3.5" />
                </button>
                <button
                  type="button"
                  class="shrink-0 rounded p-1 text-ink-faint hover:text-danger disabled:opacity-30"
                  title="删掉这条会话"
                  :disabled="!chat.canSwitchSessions"
                  @click="askDelete(session.id)"
                >
                  <Trash2 class="size-3.5" />
                </button>
              </template>
            </div>
            <p v-if="chat.sessionError" class="px-3 py-2 text-[11px] text-danger">
              {{ chat.sessionError.code }}：{{ chat.sessionError.message }}
            </p>
          </div>
        </details>
        <Button
          v-if="chat.turns.length"
          variant="ghost"
          size="sm"
          title="只清空这一页；Runtime 自己的对话历史不受影响"
          @click="chat.clear()"
        >
          <Trash2 class="size-3.5" />
          清屏
        </Button>
        <Badge
          v-if="chat.context"
          :variant="chat.compacted ? 'accent' : 'neutral'"
          :title="contextTitle"
        >
          <Shrink class="size-3.5" />
          {{ contextLine }}
          <template v-if="chat.compacted"> · 自动压缩 {{ chat.compacted }} 次</template>
        </Badge>
        <Badge :variant="stateTone">{{ RUNTIME_STATE_LABEL[runtime.state] }}</Badge>
      </div>
    </header>

    <Alert v-if="issue" tone="warn" class="mx-6 mt-4" title="现在还不能对话">
      <p class="break-words">{{ issue }}</p>
      <p v-if="runtime.ready" class="mt-1">
        <RouterLink to="/settings" class="underline">到设置里配置模型</RouterLink>
      </p>
    </Alert>

    <div
      ref="scroller"
      class="min-h-0 flex-1 overflow-y-auto px-6 py-5 scroll-slim"
      @scroll="onScroll"
    >
      <div v-if="!chat.turns.length" class="mx-auto grid h-full max-w-3xl place-items-center">
        <div class="space-y-4 text-center">
          <MessageSquare class="mx-auto size-8 text-ink-faint" />
          <h2 class="text-base font-semibold tracking-tight">和 Runtime 说点什么</h2>
          <p class="mx-auto max-w-lg text-sm text-ink-muted">
            提示词会交给 Runtime 里真实运行的 Agent：它自己决定调用哪些工具，回答、工具结果和权限拒绝都会
            逐步显示在这里。
          </p>
          <ul class="flex flex-wrap justify-center gap-2">
            <li v-for="text in SUGGESTIONS" :key="text">
              <Button variant="outline" size="sm" :disabled="!chat.canSend" @click="suggest(text)">
                {{ text }}
              </Button>
            </li>
          </ul>
        </div>
      </div>

      <ol v-else class="mx-auto flex max-w-3xl flex-col gap-6">
        <li
          v-for="turn in chat.turns"
          :key="turn.localId"
          class="space-y-3 border-l-2 pl-4"
          :class="turnTone(turn)"
        >
          <div class="flex justify-end">
            <div class="max-w-[85%] rounded-xl border border-accent/30 bg-accent/10 px-3.5 py-2">
              <p class="text-sm whitespace-pre-wrap text-ink">{{ turn.prompt }}</p>
            </div>
          </div>

          <div v-for="(block, index) in turn.blocks" :key="index" class="space-y-2">
            <p v-if="block.kind === 'text'" class="text-sm whitespace-pre-wrap text-ink">
              {{ block.text }}
            </p>

            <div
              v-else-if="block.kind === 'tool'"
              class="overflow-hidden rounded-lg border border-line bg-surface/60"
            >
              <div class="flex items-center gap-2 border-b border-line/60 px-3 py-1.5">
                <Wrench class="size-3.5 shrink-0 text-ink-faint" />
                <span class="font-mono text-xs text-ink">{{ block.name }}</span>
                <Badge v-if="block.source !== 'main'" class="px-1.5 py-0 text-[10px]">
                  子智能体
                </Badge>
                <LoaderCircle
                  v-if="block.output === null"
                  class="size-3.5 animate-spin text-ink-faint"
                />
                <Badge v-else-if="block.isError" variant="danger" class="px-1.5 py-0 text-[10px]">
                  失败
                </Badge>
              </div>
              <p
                v-if="block.input && !todos(block).length"
                class="truncate border-b border-line/60 px-3 py-1.5 font-mono text-[11px] text-ink-faint"
                :title="block.input"
              >
                {{ block.input }}
              </p>

              <!-- `todo_write` 的整份清单就在它的参数里：摊成清单比一行 JSON 好读。 -->
              <ol v-if="todos(block).length" class="divide-y divide-line/60">
                <li
                  v-for="(step, stepIndex) in todos(block)"
                  :key="stepIndex"
                  class="flex items-start gap-2 px-3 py-1.5 text-xs"
                >
                  <component
                    :is="todoIcon(step.status)"
                    class="mt-px size-3.5 shrink-0"
                    :class="
                      step.status === 'in_progress'
                        ? 'animate-spin text-accent'
                        : step.status === 'completed'
                          ? 'text-ink-faint'
                          : 'text-ink-muted'
                    "
                  />
                  <span
                    class="min-w-0 break-words"
                    :class="
                      step.status === 'completed' ? 'text-ink-faint line-through' : 'text-ink'
                    "
                  >
                    {{ step.content }}
                  </span>
                </li>
              </ol>

              <pre
                v-if="block.output !== null && !todos(block).length"
                class="max-h-64 overflow-auto px-3 py-2 font-mono text-[11px] whitespace-pre-wrap text-ink-muted scroll-slim"
              >{{ block.output }}</pre>
            </div>

            <div
              v-else-if="block.kind === 'permission'"
              class="overflow-hidden rounded-lg border border-warn/45 bg-warn/5"
            >
              <div class="flex items-center gap-2 border-b border-line/60 px-3 py-1.5">
                <ShieldAlert class="size-3.5 shrink-0 text-warn" />
                <span class="text-xs text-ink">
                  申请使用 <span class="font-mono">{{ block.name }}</span>
                </span>
                <Badge v-if="block.source !== 'main'" class="px-1.5 py-0 text-[10px]">子智能体</Badge>
                <Badge
                  :variant="decisionTone(block.decision)"
                  class="px-1.5 py-0 text-[10px]"
                >
                  {{ decisionLabel(block.decision) }}
                </Badge>
              </div>
              <p class="px-3 py-1.5 text-xs text-ink-muted">{{ block.reason }}</p>
              <p
                v-if="block.input"
                class="truncate border-b border-line/60 px-3 py-1.5 font-mono text-[11px] text-ink-faint"
                :title="block.input"
              >
                {{ block.input }}
              </p>
              <div
                v-if="block.decision === 'pending'"
                class="flex flex-wrap items-center gap-2 px-3 py-2"
              >
                <Button
                  size="sm"
                  :disabled="!chat.interactionSupported"
                  @click="decide(block.id, true)"
                >
                  允许
                </Button>
                <Button
                  variant="outline"
                  size="sm"
                  :disabled="!chat.interactionSupported"
                  @click="decide(block.id, false)"
                >
                  拒绝
                </Button>
                <span class="text-[11px] text-ink-faint">
                  Runtime 停在这里等这一句话；按停止等于没批准。
                </span>
              </div>
            </div>

            <div
              v-else-if="block.kind === 'question'"
              class="overflow-hidden rounded-lg border border-accent/40 bg-accent/5"
            >
              <div class="flex items-start gap-2 px-3 py-2">
                <MessageCircleQuestion class="mt-px size-3.5 shrink-0 text-accent" />
                <p class="min-w-0 text-sm break-words text-ink">{{ block.question }}</p>
              </div>
              <template v-if="!block.resolved">
                <div v-if="block.options.length" class="flex flex-wrap gap-2 px-3 pb-2">
                  <Button
                    v-for="option in block.options"
                    :key="option"
                    variant="outline"
                    size="sm"
                    :disabled="!chat.interactionSupported"
                    @click="answer(block.id, option)"
                  >
                    {{ option }}
                  </Button>
                </div>
                <div class="flex items-center gap-2 border-t border-line/60 px-3 py-2">
                  <input
                    v-model="answers[block.id]"
                    data-answer
                    type="text"
                    class="min-w-0 flex-1 rounded border border-line bg-canvas px-2 py-1 text-xs text-ink outline-none"
                    placeholder="也可以用自己的话回答"
                    :disabled="!chat.interactionSupported"
                    @keydown.enter.prevent="answer(block.id, answers[block.id] ?? '')"
                  />
                  <Button
                    size="sm"
                    :disabled="!chat.interactionSupported || !(answers[block.id] ?? '').trim()"
                    @click="answer(block.id, answers[block.id] ?? '')"
                  >
                    回答
                  </Button>
                </div>
              </template>
              <p v-else class="border-t border-line/60 px-3 py-1.5 text-xs text-ink-muted">
                {{
                  block.answer === null
                    ? '没有回答：提问期间没人回答，Runtime 照实告诉了模型。'
                    : `你回答：${block.answer}`
                }}
              </p>
            </div>

            <p
              v-else-if="block.kind === 'blocked'"
              class="flex items-start gap-2 text-xs text-danger"
            >
              <Ban class="mt-px size-3.5 shrink-0" />
              <span>{{ block.name }} 被权限策略拒绝：{{ block.reason }}</span>
            </p>

            <p
              v-else-if="block.kind === 'notice' && block.notice === 'compacted'"
              class="flex items-center gap-2 text-xs text-ink-faint"
              title="Runtime 自己压缩了这段对话的历史：旧的工具结果被收进文件，只留一个指路的引用。"
            >
              <Shrink class="size-3.5 shrink-0" />
              <span>
                Runtime 自动压缩了上下文<template v-if="compactionStep(block.text)">
                  （{{ compactionStep(block.text) }}）</template
                >
              </span>
            </p>

            <p
              v-else-if="block.kind === 'notice'"
              class="flex items-start gap-2 text-xs text-ink-faint"
            >
              <CircleAlert class="mt-px size-3.5 shrink-0" />
              <span>{{ block.text }}</span>
            </p>
          </div>

          <p v-if="turn.status === 'pending'" class="flex items-center gap-2 text-xs text-ink-faint">
            <LoaderCircle class="size-3.5 animate-spin" />
            Runtime 正在处理…
          </p>
          <p v-else-if="turn.status === 'failed'" class="flex items-start gap-2 text-xs text-danger">
            <CircleAlert class="mt-px size-3.5 shrink-0" />
            <span class="break-words">
              {{ turn.error?.code }}：{{ turn.error?.message }}
            </span>
          </p>
          <p
            v-else-if="turn.status === 'cancelled'"
            class="flex items-start gap-2 text-xs text-ink-faint"
          >
            <Square class="mt-0.5 size-3 shrink-0" />
            <span>这一轮被停止了；Runtime 不会把它记进上下文。</span>
          </p>
        </li>
      </ol>
    </div>

    <footer class="border-t border-line px-6 py-4">
      <div class="mx-auto flex max-w-3xl items-end gap-3">
        <textarea
          v-model="draft"
          rows="3"
          spellcheck="false"
          class="max-h-40 min-h-20 w-full resize-none rounded-lg border border-line bg-surface px-3 py-2 text-sm text-ink placeholder:text-ink-faint focus:outline-none disabled:opacity-60"
          :disabled="!chat.canSend"
          :placeholder="placeholder"
          @keydown.enter.exact.prevent="submit"
        />
        <Button v-if="chat.pending" variant="outline" :disabled="!chat.canCancel" @click="chat.cancel()">
          <Square class="size-3.5" />
          停止
        </Button>
        <Button v-else :disabled="!canSubmit" @click="submit">
          <SendHorizontal class="size-4" />
          发送
        </Button>
      </div>
      <p class="mx-auto mt-2 max-w-3xl text-[11px] text-ink-faint">
        {{ hint }} Enter 发送，Shift+Enter 换行。
      </p>
    </footer>
  </div>
</template>
