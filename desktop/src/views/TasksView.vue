<script setup lang="ts">
import {
  CircleAlert,
  CircleCheck,
  CircleDot,
  CirclePlay,
  GitBranch,
  ListChecks,
  LoaderCircle,
  LockKeyhole,
  Plus,
  RefreshCw,
  RotateCcw,
  UserRound,
} from '@lucide/vue'
import { computed, onMounted, ref } from 'vue'

import Alert from '@/components/ui/Alert.vue'
import Badge from '@/components/ui/Badge.vue'
import Button from '@/components/ui/Button.vue'
import Card from '@/components/ui/Card.vue'
import { shortenPath } from '@/lib/format'
import type { Task, TaskStatus } from '@/lib/tasks'
import { useRuntimeStore } from '@/stores/runtime'
import { useTasksStore } from '@/stores/tasks'

const tasks = useTasksStore()
const runtime = useRuntimeStore()

onMounted(() => {
  void tasks.refresh()
})

/** The form. The page names the node and the edges it read from the board. */
const subject = ref('')
const description = ref('')
const dependencies = ref<string[]>([])

/** The one honest reason the page cannot show or change the board. */
const issue = computed(() => {
  if (!runtime.shell) return '当前页面不在桌面宿主中：没有 Runtime 通道，读不到任务板。'
  if (runtime.state === 'error') {
    return runtime.lastError
      ? `${runtime.lastError.code}: ${runtime.lastError.message}`
      : 'Runtime 处于错误状态，详情见设置。'
  }
  if (!runtime.ready) return 'Runtime 还没有运行。'
  if (!tasks.supported) {
    return '这个 Runtime 没有上报 tasks.list 方法，因此看不到它的任务板。'
  }
  return null
})

const canSubmit = computed(() => tasks.canChange && subject.value.trim() !== '')

/** Nodes a new task may wait on. A completed node adds nothing to wait for. */
const candidates = computed(() =>
  tasks.ordered.filter((task) => task.status !== 'completed'),
)

const STATUS_LABEL: Record<TaskStatus, string> = {
  pending: '待办',
  in_progress: '进行中',
  completed: '已完成',
}

const STATUS_ICON = {
  pending: CircleDot,
  in_progress: CirclePlay,
  completed: CircleCheck,
} as const

function statusLabel(status: TaskStatus): string {
  return STATUS_LABEL[status] ?? status
}

function statusIcon(status: TaskStatus) {
  return STATUS_ICON[status] ?? CircleDot
}

function statusVariant(status: TaskStatus): 'neutral' | 'warn' | 'ok' {
  if (status === 'in_progress') return 'warn'
  if (status === 'completed') return 'ok'
  return 'neutral'
}

/** The owner in the words the page can stand behind. */
function ownerLabel(task: Task): string {
  if (!task.owner) return '未认领'
  return task.owner === 'user' ? '我' : task.owner
}

/** A prerequisite, named rather than only numbered when the board still has it. */
function taskSubject(id: string): string {
  return tasks.tasks.find((task) => task.id === id)?.subject ?? id
}

async function submit(): Promise<void> {
  if (!canSubmit.value) return
  const created = await tasks.create({
    subject: subject.value.trim(),
    description: description.value.trim(),
    blockedBy: dependencies.value,
  })
  if (created) {
    subject.value = ''
    description.value = ''
    dependencies.value = []
  }
}
</script>

<template>
  <div class="h-full min-h-0 overflow-y-auto p-8 scroll-slim">
    <header class="mb-5 flex items-start justify-between gap-4">
      <div class="min-w-0">
        <p class="text-xs tracking-[0.18em] text-ink-faint uppercase">Workbench</p>
        <h1 class="mt-1 text-xl font-semibold tracking-tight">任务</h1>
        <p class="mt-1 max-w-2xl text-sm text-ink-muted">
          任务板是工作区里的 <span class="font-mono">.tasks</span> 目录：模型用
          <span class="font-mono">create_task</span> /
          <span class="font-mono">claim_task</span> /
          <span class="font-mono">complete_task</span> 写的是同一张依赖图。页面可以加节点、
          认领、放回和完成，认领人记为 <span class="font-mono">user</span>，与模型的
          <span class="font-mono">agent</span> 区分开。
        </p>
      </div>
      <div class="flex shrink-0 items-center gap-2">
        <Badge v-if="tasks.loaded">
          {{ tasks.completed.length }} / {{ tasks.tasks.length }} 已完成
        </Badge>
        <Button
          variant="outline"
          size="sm"
          :disabled="!tasks.ready || tasks.loading"
          @click="tasks.refresh()"
        >
          <RefreshCw class="size-3.5" :class="tasks.loading ? 'animate-spin' : ''" />
          刷新
        </Button>
      </div>
    </header>

    <Alert v-if="issue" tone="info" class="mb-5" title="现在还看不到任务板">
      <p>{{ issue }}</p>
      <p v-if="!runtime.ready" class="text-xs">
        任务板属于 Runtime 的工作区；它不在时这一页读不到任何节点。
      </p>
    </Alert>

    <Alert v-else-if="tasks.error" tone="danger" class="mb-5" :title="tasks.error.code">
      <p>{{ tasks.error.message }}</p>
    </Alert>

    <div class="grid grid-cols-1 gap-5 xl:grid-cols-[minmax(0,21rem)_minmax(0,1fr)]">
      <Card>
        <template #header>
          <div class="min-w-0">
            <h2 class="text-base font-semibold tracking-tight">新建任务</h2>
            <p class="text-xs text-ink-faint">节点由 Runtime 分配 ID，依赖只接受板上已有的节点。</p>
          </div>
        </template>

        <form class="space-y-3" @submit.prevent="submit">
          <label class="block text-xs text-ink-faint">
            标题
            <input
              v-model="subject"
              type="text"
              class="mt-1 w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-sm text-ink placeholder:text-ink-faint focus:outline-none disabled:opacity-60"
              placeholder="例如：把失败的两个测试修好"
              :disabled="!tasks.canChange"
            />
          </label>

          <label class="block text-xs text-ink-faint">
            说明
            <textarea
              v-model="description"
              class="mt-1 max-h-40 min-h-20 w-full resize-none rounded-md border border-line bg-surface px-2.5 py-1.5 text-sm text-ink placeholder:text-ink-faint focus:outline-none disabled:opacity-60"
              placeholder="可选的上下文：为什么做、做到什么算完成"
              :disabled="!tasks.canChange"
            ></textarea>
          </label>

          <div v-if="candidates.length" class="space-y-1">
            <p class="text-xs text-ink-faint">前置任务（可选）</p>
            <div class="max-h-40 space-y-1 overflow-y-auto rounded-md border border-line/70 p-2 scroll-slim">
              <label
                v-for="candidate in candidates"
                :key="candidate.id"
                class="flex items-start gap-2 rounded px-1 py-1 text-xs text-ink-muted hover:bg-surface-raised"
              >
                <input
                  v-model="dependencies"
                  type="checkbox"
                  :value="candidate.id"
                  :disabled="!tasks.canChange"
                  class="mt-0.5"
                />
                <span class="min-w-0 flex-1 truncate">{{ candidate.subject }}</span>
                <Badge :variant="statusVariant(candidate.status)" class="px-1.5 py-0 text-[10px]">
                  {{ statusLabel(candidate.status) }}
                </Badge>
              </label>
            </div>
          </div>
          <p v-else class="text-xs text-ink-faint">板上还没有可当依赖的节点。</p>

          <Button type="submit" :disabled="!canSubmit">
            <Plus class="size-4" />
            添加任务
          </Button>
        </form>

        <template #footer>
          依赖在写入前检查；指向不存在的节点会被 Runtime 拒绝，而不是留下一个永远不动的任务。
        </template>
      </Card>

      <div class="flex min-w-0 flex-col gap-5">
        <div class="grid grid-cols-2 gap-3 sm:grid-cols-4">
          <div class="rounded-panel border border-line bg-surface px-4 py-3">
            <p class="text-xs text-ink-faint">待办</p>
            <p class="mt-1 text-lg font-semibold tracking-tight">{{ tasks.pending.length }}</p>
          </div>
          <div class="rounded-panel border border-line bg-surface px-4 py-3">
            <p class="text-xs text-ink-faint">可认领</p>
            <p class="mt-1 text-lg font-semibold tracking-tight text-accent">{{ tasks.claimable.length }}</p>
          </div>
          <div class="rounded-panel border border-line bg-surface px-4 py-3">
            <p class="text-xs text-ink-faint">等待前置</p>
            <p class="mt-1 text-lg font-semibold tracking-tight text-warn">{{ tasks.blocked.length }}</p>
          </div>
          <div class="rounded-panel border border-line bg-surface px-4 py-3">
            <p class="text-xs text-ink-faint">进行中</p>
            <p class="mt-1 text-lg font-semibold tracking-tight">{{ tasks.inProgress.length }}</p>
          </div>
        </div>

        <Card>
          <template #header>
            <div class="min-w-0">
              <h2 class="text-base font-semibold tracking-tight">任务板</h2>
              <p class="text-xs text-ink-faint">
                {{ tasks.tasks.length }} 个节点
                <span v-if="tasks.dir" class="font-mono"> · {{ shortenPath(tasks.dir, 52) }}</span>
              </p>
            </div>
          </template>

          <ul v-if="tasks.ordered.length" class="divide-y divide-line/60">
            <li
              v-for="task in tasks.ordered"
              :key="task.id"
              class="flex flex-wrap items-start justify-between gap-3 py-3 first:pt-0 last:pb-0"
            >
              <div class="min-w-0 flex-1 space-y-1.5">
                <div class="flex flex-wrap items-center gap-2">
                  <Badge :variant="statusVariant(task.status)" class="px-1.5 py-0 text-[10px]">
                    <component :is="statusIcon(task.status)" class="size-3" />
                    {{ statusLabel(task.status) }}
                  </Badge>
                  <span class="text-sm font-medium text-ink">{{ task.subject }}</span>
                  <Badge v-if="task.worktree" class="px-1.5 py-0 text-[10px]">
                    worktree {{ task.worktree }}
                  </Badge>
                </div>

                <p v-if="task.description" class="text-sm break-words whitespace-pre-wrap text-ink-muted">
                  {{ task.description }}
                </p>

                <p class="flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-ink-faint">
                  <span class="flex items-center gap-1">
                    <UserRound class="size-3" />
                    {{ ownerLabel(task) }}
                  </span>
                  <span v-if="task.blockedBy.length" class="flex min-w-0 flex-wrap items-center gap-1">
                    <GitBranch class="size-3 shrink-0" />
                    <span
                      v-for="dependency in task.blockedBy"
                      :key="dependency"
                      class="max-w-40 truncate rounded border border-line/70 px-1.5 py-0.5 font-mono"
                      :title="dependency"
                    >
                      {{ taskSubject(dependency) }}
                    </span>
                  </span>
                  <span class="font-mono">{{ task.id }}</span>
                </p>

                <p v-if="task.status === 'pending' && task.blocked.length" class="flex items-center gap-1 text-[11px] text-warn">
                  <LockKeyhole class="size-3" />
                  还差 {{ task.blocked.length }} 个前置节点
                </p>
              </div>

              <div class="flex shrink-0 items-center gap-1.5">
                <template v-if="task.status === 'pending' && task.ready">
                  <Button
                    size="sm"
                    :disabled="!tasks.canChange || tasks.busy === task.id"
                    @click="tasks.claim(task.id)"
                  >
                    <CirclePlay class="size-3.5" />
                    认领
                  </Button>
                </template>

                <template v-else-if="task.status === 'in_progress' && task.owner === 'user'">
                  <Button
                    size="sm"
                    :disabled="!tasks.canChange || tasks.busy === task.id"
                    @click="tasks.complete(task.id)"
                  >
                    <CircleCheck class="size-3.5" />
                    完成
                  </Button>
                  <Button
                    variant="ghost"
                    size="sm"
                    :disabled="!tasks.canChange || tasks.busy === task.id"
                    @click="tasks.release(task.id)"
                  >
                    <RotateCcw class="size-3.5" />
                    放回
                  </Button>
                </template>

                <span
                  v-else-if="task.status === 'in_progress'"
                  class="text-[11px] text-ink-faint"
                >
                  由 {{ ownerLabel(task) }} 处理中
                </span>

                <span v-else-if="task.status === 'pending'" class="text-[11px] text-ink-faint">
                  等待前置完成
                </span>
              </div>
            </li>
          </ul>

          <div v-else class="flex items-center gap-2 py-2 text-sm text-ink-faint">
            <ListChecks class="size-4 shrink-0" />
            <span v-if="tasks.loaded">这个工作区还没有任务。</span>
            <span v-else>还没有读到任务板。</span>
          </div>

          <template #footer>
            <span v-if="tasks.loading" class="flex items-center gap-2">
              <LoaderCircle class="size-3.5 animate-spin" />
              读取任务板…
            </span>
            <span v-else>
              只有 <span class="font-mono">user</span> 认领的任务能在这一页完成；模型或队友认领的任务由它们自己的循环结束。
            </span>
          </template>
        </Card>
      </div>
    </div>

    <p class="mt-5 flex items-start gap-2 text-xs text-ink-faint">
      <CircleAlert class="mt-px size-3.5 shrink-0" />
      <span>
        任务投递、验收和群聊还没有接入：这一页只展示并推进已有的单跳任务图，不虚构多智能体调度。
      </span>
    </p>
  </div>
</template>
