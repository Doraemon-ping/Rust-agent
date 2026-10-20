<script setup lang="ts">
import { CalendarClock, CircleAlert, Clock, LoaderCircle, Plus, RefreshCw, Trash2 } from '@lucide/vue'
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'

import Alert from '@/components/ui/Alert.vue'
import Badge from '@/components/ui/Badge.vue'
import Button from '@/components/ui/Button.vue'
import Card from '@/components/ui/Card.vue'
import type { CronJob } from '@/lib/cron'
import { formatCountdown, formatLocalStamp, formatTime } from '@/lib/format'
import { useCronStore } from '@/stores/cron'
import { useRuntimeStore } from '@/stores/runtime'

const cron = useCronStore()
const runtime = useRuntimeStore()

/**
 * A one-second clock for the countdowns.
 *
 * It is a clock, not a poller: it re-renders the same values and, once a next
 * run it was showing has passed, asks the runtime what happened to it. Nothing
 * is invented while nothing is due.
 */
const now = ref(Date.now())
let timer: ReturnType<typeof setInterval> | null = null

onMounted(() => {
  timer = setInterval(() => {
    now.value = Date.now()
    cron.refreshIfDue(now.value)
  }, 1000)
  void cron.refresh()
})
onBeforeUnmount(() => {
  if (timer !== null) clearInterval(timer)
})

/** The form. The expression and the prompt are the page's to choose. */
const expression = ref('')
const prompt = ref('')
const recurring = ref(true)
const durable = ref(true)
/** The job the list is asking about before it cancels it. */
const confirming = ref('')

const PRESETS = [
  { label: '每天 09:00', cron: '0 9 * * *' },
  { label: '每 5 分钟', cron: '*/5 * * * *' },
  { label: '工作日 09:30', cron: '30 9 * * 1-5' },
  { label: '每周一 10:00', cron: '0 10 * * 1' },
] as const

/** The one honest reason the page cannot show or change a schedule. */
const issue = computed(() => {
  if (!runtime.shell) return '当前页面不在桌面宿主中：没有 Runtime 通道，读不到定时任务。'
  if (runtime.state === 'error') {
    return runtime.lastError
      ? `${runtime.lastError.code}: ${runtime.lastError.message}`
      : 'Runtime 处于错误状态，详情见设置。'
  }
  if (!runtime.ready) return 'Runtime 还没有运行。'
  if (!cron.supported) {
    return '这个 Runtime 没有上报 cron.list 方法，因此看不到它的任务表。'
  }
  return null
})

const canSubmit = computed(
  () => cron.canChange && expression.value.trim() !== '' && prompt.value.trim() !== '',
)

/** Fill the expression field from a preset. */
function usePreset(value: string): void {
  expression.value = value
}

async function submit(): Promise<void> {
  if (!canSubmit.value) return
  const created = await cron.schedule({
    cron: expression.value.trim(),
    prompt: prompt.value.trim(),
    recurring: recurring.value,
    durable: durable.value,
  })
  if (created) {
    expression.value = ''
    prompt.value = ''
  }
}

async function confirmCancel(id: string): Promise<void> {
  if (await cron.cancel(id)) confirming.value = ''
}

/** How long until this job's next run, against the clock on screen. */
function remaining(job: CronJob): number | null {
  if (job.next_run_ms === null) return null
  return job.next_run_ms - (now.value - cron.readAt)
}

/** What the next run is, in the one word that fits its state. */
function nextLabel(job: CronJob): string {
  if (job.pending_delivery) return '等待投递'
  const left = remaining(job)
  if (left === null) return '不再运行'
  return formatCountdown(left)
}
</script>

<template>
  <div class="h-full min-h-0 overflow-y-auto p-8 scroll-slim">
    <header class="mb-5 flex items-start justify-between gap-4">
      <div class="min-w-0">
        <p class="text-xs tracking-[0.18em] text-ink-faint uppercase">Workbench</p>
        <h1 class="mt-1 text-xl font-semibold tracking-tight">定时任务</h1>
        <p class="mt-1 max-w-2xl text-sm text-ink-muted">
          到点由 Runtime 自己起一轮：到点的任务会作为 <span class="font-mono">[Scheduled]</span>
          提示词交给模型，过程和结果出现在对话页。这一页展示的是 Runtime 自己的任务表，不是另存的一份。
        </p>
      </div>
      <div class="flex shrink-0 items-center gap-2">
        <Badge v-if="cron.waiting > 0" variant="warn">{{ cron.waiting }} 个待投递</Badge>
        <Button
          variant="outline"
          size="sm"
          :disabled="!cron.ready || cron.loading"
          @click="cron.refresh()"
        >
          <RefreshCw class="size-3.5" :class="cron.loading ? 'animate-spin' : ''" />
          刷新
        </Button>
      </div>
    </header>

    <Alert v-if="issue" tone="info" class="mb-5" title="现在还看不到任务表">
      <p>{{ issue }}</p>
      <p v-if="!runtime.ready" class="text-xs">
        Runtime 在运行期间提供时钟；它不在时这一页读不到任何东西，也改不了任何东西。
      </p>
    </Alert>
    <Alert v-else-if="cron.error" tone="danger" class="mb-5" title="Runtime 拒绝了这次请求">
      <p class="font-mono text-xs">{{ cron.error.code }}</p>
      <p class="break-words">{{ cron.error.message }}</p>
    </Alert>

    <div class="grid grid-cols-1 gap-5 xl:grid-cols-[24rem_1fr]">
      <Card>
        <template #header>
          <h2 class="text-base font-semibold tracking-tight">新建任务</h2>
        </template>
        <form class="space-y-3" @submit.prevent="submit">
          <label class="block space-y-1">
            <span class="text-xs text-ink-faint">cron 表达式（分 时 日 月 周，本地时间）</span>
            <input
              v-model="expression"
              spellcheck="false"
              class="w-full rounded-md border border-line bg-surface px-2.5 py-1.5 font-mono text-sm text-ink placeholder:text-ink-faint focus:outline-none disabled:opacity-60"
              placeholder="0 9 * * 1-5"
              :disabled="!cron.canChange"
            />
          </label>
          <div class="flex flex-wrap gap-1.5">
            <Button
              v-for="preset in PRESETS"
              :key="preset.cron"
              variant="outline"
              size="sm"
              :disabled="!cron.canChange"
              :title="preset.cron"
              @click="usePreset(preset.cron)"
            >
              {{ preset.label }}
            </Button>
          </div>

          <label class="block space-y-1">
            <span class="text-xs text-ink-faint">提示词</span>
            <textarea
              v-model="prompt"
              rows="3"
              spellcheck="false"
              class="max-h-40 min-h-20 w-full resize-none rounded-md border border-line bg-surface px-2.5 py-1.5 text-sm text-ink placeholder:text-ink-faint focus:outline-none disabled:opacity-60"
              placeholder="例如：跑一遍测试，把失败的那几条列出来"
              :disabled="!cron.canChange"
            />
          </label>

          <div class="space-y-2 text-xs text-ink-muted">
            <label class="flex items-center gap-2">
              <input v-model="recurring" type="checkbox" :disabled="!cron.canChange" />
              每次匹配都运行（关闭则只运行一次，到点后自动移除）
            </label>
            <label class="flex items-center gap-2">
              <input v-model="durable" type="checkbox" :disabled="!cron.canChange" />
              持久保存（Runtime 重启后仍在）
            </label>
          </div>

          <Button type="submit" :disabled="!canSubmit">
            <Plus class="size-4" />
            添加任务
          </Button>
        </form>

        <template #footer>
          表达式由 Runtime 校验；写错会带着它的原话被拒绝，而不是留一个永远不会触发的任务。
        </template>
      </Card>

      <Card>
        <template #header>
          <div class="min-w-0">
            <h2 class="text-base font-semibold tracking-tight">任务表</h2>
            <p class="text-xs text-ink-faint">
              {{ cron.jobs.length }} 个任务
              <span v-if="cron.readAt"> · 读取于 {{ formatTime(new Date(cron.readAt).toISOString()) }}</span>
            </p>
          </div>
        </template>

        <ul v-if="cron.jobs.length" class="divide-y divide-line/60">
          <li
            v-for="job in cron.jobs"
            :key="job.id"
            class="flex flex-wrap items-start justify-between gap-3 py-3 first:pt-0 last:pb-0"
          >
            <div class="min-w-0 flex-1 space-y-1.5">
              <div class="flex flex-wrap items-center gap-2">
                <span class="font-mono text-sm text-ink">{{ job.cron }}</span>
                <Badge class="px-1.5 py-0 text-[10px]">
                  {{ job.recurring ? '重复' : '一次' }}
                </Badge>
                <Badge class="px-1.5 py-0 text-[10px]">
                  {{ job.durable ? '持久' : '会话' }}
                </Badge>
                <Badge v-if="job.pending_delivery" variant="warn" class="px-1.5 py-0 text-[10px]">
                  待投递
                </Badge>
              </div>
              <p class="text-sm break-words whitespace-pre-wrap text-ink-muted">{{ job.prompt }}</p>
              <p class="flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-ink-faint">
                <span class="flex items-center gap-1">
                  <Clock class="size-3" />
                  下一次 {{ nextLabel(job) }}
                  <span v-if="job.next_run" class="font-mono">（{{ formatLocalStamp(job.next_run) }}）</span>
                </span>
                <span v-if="job.last_fired" class="font-mono">上次 {{ job.last_fired }}</span>
                <span class="font-mono">{{ job.id }}</span>
              </p>
            </div>

            <div class="flex shrink-0 items-center gap-1.5">
              <template v-if="confirming === job.id">
                <Button
                  variant="danger"
                  size="sm"
                  :disabled="!cron.canChange"
                  @click="confirmCancel(job.id)"
                >
                  确认移除
                </Button>
                <Button variant="ghost" size="sm" @click="confirming = ''">返回</Button>
              </template>
              <Button
                v-else
                variant="ghost"
                size="sm"
                :disabled="!cron.canChange"
                @click="confirming = job.id"
              >
                <Trash2 class="size-3.5" />
                移除
              </Button>
            </div>
          </li>
        </ul>

        <div v-else class="space-y-3 py-2 text-sm text-ink-faint">
          <div class="flex items-center gap-2">
            <CalendarClock class="size-4 shrink-0" />
            <span v-if="cron.loaded">这个工作区还没有定时任务。</span>
            <span v-else>还没有读到任务表。</span>
          </div>
          <div v-if="cron.ready && cron.loaded" class="flex flex-wrap gap-1.5">
            <Button
              v-for="preset in PRESETS"
              :key="preset.cron"
              variant="outline"
              size="sm"
              @click="usePreset(preset.cron)"
            >
              {{ preset.label }}
            </Button>
          </div>
        </div>

        <template #footer>
          <span v-if="cron.loading" class="flex items-center gap-2">
            <LoaderCircle class="size-3.5 animate-spin" />
            读取任务表…
          </span>
          <span v-else>
            任务在 Runtime 运行期间到点就触发；应用关闭时 Runtime 一起退出，错过的时间点不会补跑，持久任务留到下次运行。
          </span>
        </template>
      </Card>
    </div>

    <p class="mt-5 flex items-start gap-2 text-xs text-ink-faint">
      <CircleAlert class="mt-px size-3.5 shrink-0" />
      <span>
        一次只能有一轮在跑：到点的任务会占住对话的那一轮，对话页能看到它，也可以在那里停止它。
      </span>
    </p>
  </div>
</template>
