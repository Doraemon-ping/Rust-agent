<script setup lang="ts">
import { computed } from 'vue'

import Badge from '@/components/ui/Badge.vue'
import Card from '@/components/ui/Card.vue'
import Alert from '@/components/ui/Alert.vue'
import { formatDuration, formatRelative, formatTime, shortenPath } from '@/lib/format'
import { RUNTIME_STATE_LABEL } from '@/lib/runtime/types'
import { useRuntimeStore } from '@/stores/runtime'

const props = defineProps<{ now: number }>()

const runtime = useRuntimeStore()
const snapshot = computed(() => runtime.snapshot)

const tone = computed(() => {
  switch (runtime.state) {
    case 'ready':
      return 'ok'
    case 'error':
      return 'danger'
    case 'starting':
    case 'stopping':
      return 'warn'
    default:
      return 'neutral'
  }
})

/**
 * Facts straight off `runtime.status`; nothing here is invented.
 *
 * Uptime is measured from the runtime's own `started_at` against the ticking
 * clock rather than read from `uptime_ms`: the snapshot is captured at the
 * handshake, so its uptime would freeze at "0 s" forever.
 */
const facts = computed(() => {
  const data = snapshot.value
  if (!data) return []
  return [
    { label: '进程 PID', value: String(data.pid) },
    { label: '已运行', value: formatDuration(props.now - Date.parse(data.started_at)) },
    { label: '启动于', value: formatTime(data.started_at) },
    { label: '模型', value: data.model ?? '未配置' },
    { label: '技能', value: `${data.skills.count}` },
    { label: '任务', value: `${data.tasks.total}（待办 ${data.tasks.pending}）` },
    { label: '记忆', value: `${data.memory.records}` },
    { label: '定时任务', value: `${data.cron.jobs}` },
  ]
})
</script>

<template>
  <Card>
    <template #header>
      <div class="min-w-0 space-y-1">
        <h2 class="text-base font-semibold tracking-tight">Runtime 状态</h2>
        <p class="text-xs text-ink-faint">
          状态变化于 {{ formatRelative(runtime.status.changedAt, props.now) }}（{{
            formatTime(runtime.status.changedAt)
          }}）
        </p>
      </div>
      <Badge :variant="tone">{{ RUNTIME_STATE_LABEL[runtime.state] }}</Badge>
    </template>

    <div class="space-y-4">
      <Alert v-if="runtime.lastError" tone="danger" title="Runtime 错误">
        <p class="font-mono text-xs">{{ runtime.lastError.code }}</p>
        <p class="break-words">{{ runtime.lastError.message }}</p>
      </Alert>

      <Alert
        v-if="snapshot?.config_error"
        tone="warn"
        title="未配置模型凭据"
      >
        <p>{{ snapshot.config_error }}</p>
        <p class="text-xs">
          第一阶段的方法不需要模型，因此 Runtime 仍可正常连接。
        </p>
      </Alert>

      <Alert v-if="snapshot?.warnings?.length" tone="warn" title="Runtime 报告了警告">
        <ul class="list-disc space-y-0.5 pl-4">
          <li v-for="warning in snapshot.warnings" :key="warning">{{ warning }}</li>
        </ul>
      </Alert>

      <dl v-if="facts.length" class="grid grid-cols-2 gap-x-6 gap-y-3 sm:grid-cols-4">
        <div v-for="fact in facts" :key="fact.label" class="min-w-0 space-y-0.5">
          <dt class="text-xs text-ink-faint">{{ fact.label }}</dt>
          <dd class="truncate font-mono text-sm text-ink" :title="fact.value">
            {{ fact.value }}
          </dd>
        </div>
      </dl>

      <p v-else class="text-sm text-ink-faint">
        运行时未上报状态快照。
        <span v-if="runtime.state === 'starting'">正在启动…</span>
      </p>
    </div>

    <template #footer>
      <div class="space-y-1">
        <p class="truncate" :title="snapshot?.workspace ?? ''">
          工作目录 {{ shortenPath(snapshot?.workspace ?? '--', 64) }}
        </p>
        <p v-if="runtime.status.bridgePath" class="truncate" :title="runtime.status.bridgePath">
          Runtime 可执行文件 {{ shortenPath(runtime.status.bridgePath, 64) }}
        </p>
      </div>
    </template>
  </Card>
</template>
