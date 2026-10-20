<script setup lang="ts">
import {
  Bot,
  Building2,
  CheckCircle2,
  CirclePlay,
  ListChecks,
  LoaderCircle,
  MessageSquare,
  Plus,
  RefreshCw,
  Square,
  Trash2,
  UserRound,
  Users,
} from '@lucide/vue'
import { computed, onMounted, ref } from 'vue'
import { RouterLink } from 'vue-router'

import Alert from '@/components/ui/Alert.vue'
import Badge from '@/components/ui/Badge.vue'
import Button from '@/components/ui/Button.vue'
import Card from '@/components/ui/Card.vue'
import type { AgentProfile } from '@/lib/departments'
import { formatRelative, formatTime, shortenPath } from '@/lib/format'
import { useDepartmentsStore } from '@/stores/departments'
import { useRuntimeStore } from '@/stores/runtime'

const departments = useDepartmentsStore()
const runtime = useRuntimeStore()

onMounted(() => {
  void departments.refresh()
})

const selected = computed(() => departments.selected)
const currentAgent = computed(() => {
  const shift = selected.value?.shift
  if (!shift?.currentAgentId) return null
  return selected.value?.agents.find((agent) => agent.id === shift.currentAgentId) ?? null
})
const running = computed(() => Boolean(selected.value?.shift))

const issue = computed(() => {
  if (!runtime.shell) return '当前页面不在桌面宿主中：没有 Runtime 通道，读不到部门。'
  if (runtime.state === 'error') {
    return runtime.lastError
      ? `${runtime.lastError.code}: ${runtime.lastError.message}`
      : 'Runtime 处于错误状态，详情见设置。'
  }
  if (!runtime.ready) return 'Runtime 还没有运行。'
  if (!departments.supported) {
    return '这个 Runtime 没有上报 departments.list 方法，因此看不到部门。'
  }
  return null
})

/** New department form. */
const showDepartmentForm = ref(false)
const departmentName = ref('')
const departmentDescription = ref('')
const departmentInstructions = ref('')
const parentId = ref('')

function openDepartmentForm(parent: string | null = null): void {
  showDepartmentForm.value = true
  parentId.value = parent ?? ''
  departmentName.value = ''
  departmentDescription.value = ''
  departmentInstructions.value = ''
}

async function submitDepartment(): Promise<void> {
  if (departments.busy !== '' || departmentName.value.trim() === '') return
  const created = await departments.create({
    name: departmentName.value.trim(),
    description: departmentDescription.value.trim(),
    instructions: departmentInstructions.value.trim(),
    parentId: parentId.value === '' ? null : parentId.value,
  })
  if (created) showDepartmentForm.value = false
}

/** Add-agent form, scoped to the selected department. */
const agentName = ref('')
const agentRole = ref('')
const agentInstructions = ref('')

async function submitAgent(): Promise<void> {
  if (!selected.value || departments.busy !== '' || agentName.value.trim() === '') return
  const added = await departments.addMember(selected.value.id, {
    name: agentName.value.trim(),
    role: agentRole.value.trim(),
    instructions: agentInstructions.value.trim(),
  })
  if (added) {
    agentName.value = ''
    agentRole.value = ''
    agentInstructions.value = ''
  }
}

/** The shift instruction written into the group chat. */
const instruction = ref('')

async function runSelected(): Promise<void> {
  if (!selected.value || departments.busy !== '') return
  const started = await departments.run(selected.value.id, instruction.value.trim())
  if (started) instruction.value = ''
}

async function stopSelected(): Promise<void> {
  if (!selected.value) return
  await departments.stop(selected.value.id)
}

async function removeSelectedDepartment(): Promise<void> {
  if (!selected.value) return
  const name = selected.value.name
  if (!window.confirm(`删除「${name}」及其所有子部门？任务文件会保留。`)) return
  await departments.remove(selected.value.id)
}

async function removeMember(agent: AgentProfile): Promise<void> {
  if (!selected.value) return
  if (!window.confirm(`从「${selected.value.name}」移除成员「${agent.name}」？`)) return
  await departments.removeMember(selected.value.id, agent.id)
}

function statusVariant(status: string | null): 'neutral' | 'ok' | 'warn' | 'danger' {
  if (status === 'completed') return 'ok'
  if (status === 'failed') return 'danger'
  if (status === 'stopped' || status === 'empty') return 'warn'
  return 'neutral'
}

function statusLabel(status: string | null): string {
  switch (status) {
    case 'completed':
      return '已完成'
    case 'failed':
      return '失败'
    case 'stopped':
      return '已停止'
    case 'empty':
      return '无产出'
    default:
      return '未上班'
  }
}

function initials(name: string): string {
  return [...name.trim()].slice(0, 2).join('') || '?'
}
</script>

<template>
  <div
    class="@container flex h-full min-h-0 flex-col gap-5 overflow-x-hidden overflow-y-auto p-4 scroll-slim sm:p-6 xl:p-8"
  >
    <header class="flex flex-wrap items-start justify-between gap-4">
      <div class="min-w-0">
        <p class="text-xs tracking-[0.18em] text-ink-faint uppercase">Workforce</p>
        <h1 class="mt-1 text-xl font-semibold tracking-tight">部门</h1>
        <p class="mt-1 max-w-3xl text-sm text-ink-muted">
          部门是持久化的工作组：可以建分部门、加成员，并在群聊里下达本轮目标。开工后成员按顺序自主
          <span class="font-mono">create_task</span> / <span class="font-mono">claim_task</span> /
          <span class="font-mono">complete_task</span>，所有节点都带部门标记。
        </p>
      </div>
      <div class="flex shrink-0 items-center gap-2">
        <Badge v-if="departments.loaded">
          {{ departments.departments.length }} 部门 · {{ departments.totalAgents }} 成员
        </Badge>
        <Button
          variant="outline"
          size="sm"
          :disabled="!departments.ready || departments.loading"
          @click="departments.refresh()"
        >
          <RefreshCw class="size-3.5" :class="departments.loading ? 'animate-spin' : ''" />
          刷新
        </Button>
      </div>
    </header>

    <Alert v-if="issue" tone="info" title="现在还看不到部门">
      <p>{{ issue }}</p>
      <p v-if="!runtime.ready" class="text-xs">
        部门和群聊属于 Runtime 的工作区；它不在时这一页读不到任何记录。
      </p>
    </Alert>

    <Alert v-else-if="departments.error" tone="danger" :title="departments.error.code">
      <p>{{ departments.error.message }}</p>
    </Alert>

    <div
      v-if="!issue"
      class="grid grid-cols-1 gap-5 @3xl:grid-cols-[minmax(0,17rem)_minmax(0,1fr)] @4xl:min-h-0 @4xl:flex-1 @4xl:grid-cols-[minmax(0,17rem)_minmax(0,21rem)_minmax(0,1fr)]"
    >
      <div class="flex min-w-0 flex-col gap-5 @4xl:min-h-0">
        <Card class="min-w-0 @4xl:min-h-0">
          <template #header>
            <div class="min-w-0">
              <h2 class="text-base font-semibold tracking-tight">组织结构</h2>
              <p class="text-xs text-ink-faint">部门、分部门和它们的工作状态。</p>
            </div>
            <Button size="sm" variant="outline" :disabled="!departments.canChange" @click="openDepartmentForm()">
              <Plus class="size-3.5" />
              新建
            </Button>
          </template>

          <div v-if="departments.rows.length" class="max-h-[26rem] space-y-1 overflow-y-auto pr-1 scroll-slim">
            <button
              v-for="row in departments.rows"
              :key="row.department.id"
              type="button"
              class="flex w-full items-start gap-2 rounded-md px-2 py-2 text-left transition-colors"
              :class="
                row.department.id === departments.selectedId
                  ? 'bg-accent/10 text-ink'
                  : 'text-ink-muted hover:bg-surface-raised hover:text-ink'
              "
              :style="{ paddingLeft: `${8 + row.depth * 16}px` }"
              @click="departments.select(row.department.id)"
            >
              <Building2 class="mt-0.5 size-4 shrink-0" :class="row.depth ? 'text-ink-faint' : 'text-accent'" />
              <span class="min-w-0 flex-1">
                <span class="flex items-center gap-1.5">
                  <span class="truncate text-sm font-medium">{{ row.department.name }}</span>
                  <Badge v-if="row.department.shift" variant="warn" class="px-1.5 py-0 text-[10px]">
                    工作中
                  </Badge>
                </span>
                <span class="mt-0.5 flex items-center gap-2 text-[11px] text-ink-faint">
                  <span>{{ row.department.agents.length }} 人</span>
                  <span>{{ row.department.tasks.completed }}/{{ row.department.tasks.total }} 任务</span>
                </span>
              </span>
            </button>
          </div>
          <div v-else class="flex items-center gap-2 py-3 text-sm text-ink-faint">
            <Building2 class="size-4" />
            <span>{{ departments.loaded ? '还没有部门。' : '正在读取部门…' }}</span>
          </div>

          <template #footer>
            <span v-if="departments.dir" class="font-mono">{{ shortenPath(departments.dir, 46) }}</span>
            <span v-else>部门保存在工作区的 .departments 目录。</span>
          </template>
        </Card>

        <Card v-if="showDepartmentForm">
          <template #header>
            <div>
              <h2 class="text-base font-semibold tracking-tight">新建部门</h2>
              <p class="text-xs text-ink-faint">父部门留空即为顶层部门。</p>
            </div>
          </template>
          <form class="space-y-3" @submit.prevent="submitDepartment">
            <label class="block text-xs text-ink-faint">
              名称
              <input
                v-model="departmentName"
                type="text"
                class="mt-1 w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-sm text-ink placeholder:text-ink-faint focus:outline-none"
                placeholder="例如：产品研发部"
                :disabled="!departments.canChange"
              />
            </label>
            <label class="block text-xs text-ink-faint">
              上级部门
              <select
                v-model="parentId"
                class="mt-1 w-full rounded-md border border-line bg-surface px-2.5 py-1.5 text-sm text-ink focus:outline-none"
                :disabled="!departments.canChange"
              >
                <option value="">无（顶层部门）</option>
                <option v-for="department in departments.departments" :key="department.id" :value="department.id">
                  {{ department.name }}
                </option>
              </select>
            </label>
            <label class="block text-xs text-ink-faint">
              部门简介
              <textarea
                v-model="departmentDescription"
                class="mt-1 min-h-16 w-full resize-none rounded-md border border-line bg-surface px-2.5 py-1.5 text-sm text-ink placeholder:text-ink-faint focus:outline-none"
                placeholder="这个部门负责什么、做到什么算好"
                :disabled="!departments.canChange"
              ></textarea>
            </label>
            <label class="block text-xs text-ink-faint">
              协作规则
              <textarea
                v-model="departmentInstructions"
                class="mt-1 min-h-16 w-full resize-none rounded-md border border-line bg-surface px-2.5 py-1.5 text-sm text-ink placeholder:text-ink-faint focus:outline-none"
                placeholder="所有成员每轮都会带上的约定"
                :disabled="!departments.canChange"
              ></textarea>
            </label>
            <div class="flex gap-2">
              <Button type="submit" :disabled="!departments.canChange || departmentName.trim() === ''">
                <Plus class="size-4" />
                创建
              </Button>
              <Button variant="ghost" @click="showDepartmentForm = false">取消</Button>
            </div>
          </form>
        </Card>

        <Card v-if="selected" class="min-w-0 @4xl:min-h-0">
          <template #header>
            <div class="min-w-0">
              <h2 class="truncate text-base font-semibold tracking-tight">{{ selected.name }}</h2>
              <p class="text-xs text-ink-faint">{{ selected.parentId ? '分部门' : '顶层部门' }}</p>
            </div>
            <div class="flex items-center gap-1.5">
              <Button size="sm" variant="outline" :disabled="!departments.canChange" @click="openDepartmentForm(selected.id)">
                <Plus class="size-3.5" />
                子部门
              </Button>
              <Button
                size="sm"
                variant="danger"
                :disabled="!departments.canChange || running"
                @click="removeSelectedDepartment"
              >
                <Trash2 class="size-3.5" />
              </Button>
            </div>
          </template>
          <p v-if="selected.description" class="text-sm break-words whitespace-pre-wrap text-ink-muted">
            {{ selected.description }}
          </p>
          <p v-else class="text-sm text-ink-faint">还没有部门简介。</p>
          <p v-if="selected.instructions" class="mt-3 rounded-md border border-line/70 px-3 py-2 text-xs break-words whitespace-pre-wrap text-ink-faint">
            {{ selected.instructions }}
          </p>
          <div class="mt-3 flex flex-wrap gap-2">
            <Badge variant="neutral">待办 {{ selected.tasks.pending }}</Badge>
            <Badge variant="warn">进行中 {{ selected.tasks.inProgress }}</Badge>
            <Badge variant="ok">完成 {{ selected.tasks.completed }}</Badge>
            <Badge variant="accent">可认领 {{ selected.tasks.ready }}</Badge>
          </div>
          <template #footer>
            <RouterLink to="/tasks" class="inline-flex items-center gap-1 text-accent hover:underline">
              <ListChecks class="size-3.5" />
              打开任务板
            </RouterLink>
          </template>
        </Card>
      </div>

      <Card v-if="selected" class="min-w-0 @4xl:min-h-0">
        <template #header>
          <div class="min-w-0">
            <h2 class="flex items-center gap-2 text-base font-semibold tracking-tight">
              <Users class="size-4" />
              成员
            </h2>
            <p class="text-xs text-ink-faint">开工时按列表顺序依次上班。</p>
          </div>
          <Badge>{{ selected.agents.length }} 人</Badge>
        </template>

        <Alert
          v-if="departments.error"
          tone="danger"
          :title="departments.error.code"
          class="mb-3"
        >
          <p class="break-words">{{ departments.error.message }}</p>
        </Alert>

        <form
          class="mb-4 space-y-3 rounded-lg border border-line/70 bg-surface-raised/40 p-3"
          aria-label="添加 Agent"
          @submit.prevent="submitAgent"
        >
          <p class="flex items-center gap-1.5 text-xs font-medium text-ink-muted">
            <Plus class="size-3.5" />
            添加 Agent
          </p>
          <div class="grid grid-cols-1 gap-2 sm:grid-cols-2">
            <input
              v-model="agentName"
              type="text"
              aria-label="Agent 名字"
              autocomplete="off"
              class="min-w-0 rounded-md border border-line bg-surface px-2.5 py-1.5 text-sm text-ink placeholder:text-ink-faint focus:outline-none"
              placeholder="名字，如 后端"
              :disabled="!departments.canChange || running"
            />
            <input
              v-model="agentRole"
              type="text"
              aria-label="Agent 职责"
              autocomplete="off"
              class="min-w-0 rounded-md border border-line bg-surface px-2.5 py-1.5 text-sm text-ink placeholder:text-ink-faint focus:outline-none"
              placeholder="职责，如 API / 测试"
              :disabled="!departments.canChange || running"
            />
          </div>
          <textarea
            v-model="agentInstructions"
            aria-label="Agent 额外要求"
            class="min-h-14 w-full resize-none rounded-md border border-line bg-surface px-2.5 py-1.5 text-sm text-ink placeholder:text-ink-faint focus:outline-none"
            placeholder="这个成员的额外要求（可选）"
            :disabled="!departments.canChange || running"
          ></textarea>
          <div class="flex items-center justify-between gap-3">
            <p class="text-[11px] text-ink-faint">名字必填，职责和建议可选。</p>
            <Button
              type="submit"
              size="sm"
              :disabled="!departments.canChange || running || agentName.trim() === ''"
            >
              <Plus class="size-3.5" />
              {{ departments.busy === `agent:${selected.id}` ? '添加中…' : '加入部门' }}
            </Button>
          </div>
        </form>

        <div v-if="selected.agents.length" class="max-h-[24rem] space-y-2 overflow-y-auto pr-1 scroll-slim">
          <div
            v-for="agent in selected.agents"
            :key="agent.id"
            class="rounded-md border border-line/70 px-3 py-2"
          >
            <div class="flex items-start gap-2">
              <span class="grid size-8 shrink-0 place-items-center rounded-full border border-accent/30 bg-accent/10 text-xs font-semibold text-accent">
                {{ initials(agent.name) }}
              </span>
              <div class="min-w-0 flex-1">
                <div class="flex flex-wrap items-center gap-1.5">
                  <span class="truncate text-sm font-medium">{{ agent.name }}</span>
                  <Badge :variant="statusVariant(agent.lastStatus)" class="px-1.5 py-0 text-[10px]">
                    {{ statusLabel(agent.lastStatus) }}
                  </Badge>
                  <span v-if="selected.shift?.currentAgentId === agent.id" class="text-[11px] text-warn">
                    正在工作
                  </span>
                </div>
                <p class="text-[11px] text-ink-faint">
                  {{ agent.role || '通才' }}
                  <span v-if="agent.lastRunAt"> · {{ formatRelative(agent.lastRunAt) }}</span>
                </p>
                <p v-if="agent.lastSummary" class="mt-1 line-clamp-2 text-xs text-ink-muted">
                  {{ agent.lastSummary }}
                </p>
                <p v-if="agent.lastCreated !== null || agent.lastCompleted !== null" class="mt-1 text-[11px] text-ink-faint">
                  上轮：新建 {{ agent.lastCreated ?? 0 }}，完成 {{ agent.lastCompleted ?? 0 }}
                </p>
              </div>
              <Button
                size="icon"
                variant="ghost"
                :disabled="!departments.canChange || Boolean(selected.shift)"
                title="移除成员"
                aria-label="移除成员"
                @click="removeMember(agent)"
              >
                <Trash2 class="size-3.5" />
              </Button>
            </div>
          </div>
        </div>
        <div v-else class="flex items-center gap-2 py-3 text-sm text-ink-faint">
          <Bot class="size-4" />
          <span>这个部门还没有成员。先加一个 Agent 才能开工。</span>
        </div>

      </Card>

      <Card
        v-if="selected"
        class="flex h-[32rem] min-w-0 flex-col overflow-hidden @3xl:col-span-2 @3xl:h-[28rem] @4xl:col-span-1 @4xl:h-auto @4xl:min-h-0"
      >
        <template #header>
          <div class="min-w-0">
            <h2 class="flex items-center gap-2 text-base font-semibold tracking-tight">
              <MessageSquare class="size-4" />
              群聊
            </h2>
            <p class="text-xs text-ink-faint">
              {{ selected.name }} · {{ selected.messages.length }} 条记录
            </p>
          </div>
          <div class="flex items-center gap-2">
            <Badge v-if="selected.shift" variant="warn">
              第 {{ Math.min(selected.shift.agentsDone + 1, selected.shift.agentsTotal) }}/{{
                selected.shift.agentsTotal
              }} 位
              <span v-if="currentAgent"> · {{ currentAgent.name }}</span>
            </Badge>
            <Badge v-else variant="neutral">未开工</Badge>
          </div>
        </template>

        <div class="min-h-0 flex-1 space-y-3 overflow-y-auto pr-1 scroll-slim">
          <div
            v-for="message in selected.messages"
            :key="message.id"
            class="flex"
            :class="message.senderKind === 'user' ? 'justify-end' : 'justify-start'"
          >
            <div
              v-if="message.senderKind === 'system'"
              class="mx-auto flex max-w-[82%] items-center gap-2 rounded-full border border-line/70 px-3 py-1 text-[11px] text-ink-faint"
            >
              <CheckCircle2 class="size-3" />
              {{ message.content }}
            </div>
            <div
              v-else
              class="max-w-[86%] rounded-lg border px-3 py-2"
              :class="
                message.senderKind === 'user'
                  ? 'border-accent/30 bg-accent/10'
                  : 'border-line bg-surface-raised'
              "
            >
              <div class="mb-1 flex items-center gap-2 text-[11px] text-ink-faint">
                <span class="font-medium text-ink-muted">{{ message.senderName }}</span>
                <span>{{ formatTime(message.createdAt) }}</span>
              </div>
              <p class="text-sm break-words whitespace-pre-wrap text-ink">{{ message.content }}</p>
            </div>
          </div>

          <div v-if="!selected.messages.length" class="flex h-full min-h-48 flex-col items-center justify-center gap-2 text-center text-sm text-ink-faint">
            <MessageSquare class="size-6" />
            <p>还没有对话。</p>
            <p class="max-w-sm text-xs">
              在下面写下本轮目标。开工后，每个成员会自己建任务、认领并完成，然后把报告发回这里。
            </p>
          </div>
        </div>

        <form class="mt-4 space-y-2 border-t border-line pt-4" @submit.prevent="runSelected">
          <textarea
            v-model="instruction"
            class="min-h-16 w-full resize-none rounded-md border border-line bg-surface px-3 py-2 text-sm text-ink placeholder:text-ink-faint focus:outline-none"
            placeholder="给部门的本轮指令，例如：补齐部门任务闭环并跑通测试。留空则按部门简介执行。"
            :disabled="!departments.ready || running"
          ></textarea>
          <div class="flex items-center justify-between gap-2">
            <p class="text-[11px] text-ink-faint">
              <span v-if="selected.agents.length === 0">先添加成员。</span>
              <span v-else-if="selected.shift">正在按顺序工作，可随时停止。</span>
              <span v-else>每个成员最多推进一张任务，完成后写回群聊。</span>
            </p>
            <div class="flex items-center gap-2">
              <Button
                v-if="running"
                variant="danger"
                size="sm"
                :disabled="departments.busy !== ''"
                @click="stopSelected"
              >
                <Square class="size-3.5" />
                停止
              </Button>
              <Button
                v-else
                type="submit"
                size="sm"
                :disabled="!departments.canChange || selected.agents.length === 0"
              >
                <CirclePlay class="size-3.5" />
                开工
              </Button>
            </div>
          </div>
        </form>

        <template #footer>
          <span class="flex items-center gap-2">
            <LoaderCircle v-if="departments.loading" class="size-3.5 animate-spin" />
            <span v-if="departments.loading">读取部门…</span>
            <span v-else>
              群聊和成员状态来自 <span class="font-mono">.departments/{{ selected.id }}.json</span>；
              任务来自 <span class="font-mono">.tasks</span>。
            </span>
          </span>
        </template>
      </Card>

      <Card v-else class="grid place-items-center text-sm text-ink-faint">
        <div class="flex flex-col items-center gap-2 text-center">
          <UserRound class="size-6" />
          <p>选择或新建一个部门开始。</p>
        </div>
      </Card>
    </div>
  </div>
</template>
