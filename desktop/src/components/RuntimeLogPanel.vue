<script setup lang="ts">
import { computed, ref } from 'vue'

import Badge from '@/components/ui/Badge.vue'
import Button from '@/components/ui/Button.vue'
import Card from '@/components/ui/Card.vue'
import { formatTime } from '@/lib/format'
import { useRuntimeStore } from '@/stores/runtime'

const runtime = useRuntimeStore()
const showEvents = ref(false)

const LEVEL_CLASS = {
  info: 'text-ink-muted',
  warn: 'text-warn',
  error: 'text-danger',
  raw: 'text-ink-faint',
} as const

const lines = computed(() =>
  showEvents.value
    ? runtime.events.map((event) => ({
        key: `${event.ts}-${event.event}`,
        ts: event.ts,
        level: 'info' as const,
        message: `${event.event} ${JSON.stringify(event.data ?? {})}`,
      }))
    : runtime.logs.map((line) => ({
        key: `${line.ts}-${line.message}`,
        ts: line.ts,
        level: line.level,
        message: line.message,
      })),
)
</script>

<template>
  <Card class="flex min-h-0 flex-col">
    <template #header>
      <div class="min-w-0 space-y-1">
        <h2 class="text-base font-semibold tracking-tight">诊断</h2>
        <p class="text-xs text-ink-faint">
          错误 {{ runtime.recentErrors.length }} 条 · 日志 {{ runtime.logs.length }} 行 ·
          事件 {{ runtime.events.length }} 条
        </p>
      </div>
      <div class="flex items-center gap-2">
        <Badge :variant="showEvents ? 'accent' : 'neutral'">
          {{ showEvents ? '协议事件' : 'stderr 日志' }}
        </Badge>
        <Button size="sm" variant="ghost" @click="showEvents = !showEvents">
          切换
        </Button>
        <Button size="sm" variant="ghost" @click="runtime.refresh()">刷新</Button>
      </div>
    </template>

    <ul
      v-if="runtime.recentErrors.length"
      class="mb-4 space-y-1 rounded-lg border border-danger/30 bg-danger/5 px-3 py-2"
    >
      <li
        v-for="line in runtime.recentErrors"
        :key="`${line.ts}-${line.message}`"
        class="flex gap-3 font-mono text-xs text-danger"
      >
        <span class="shrink-0 text-danger/70">{{ formatTime(line.ts) }}</span>
        <span class="min-w-0 break-words">{{ line.message }}</span>
      </li>
    </ul>

    <div class="min-h-0 flex-1 overflow-y-auto scroll-slim">
      <p v-if="!lines.length" class="py-6 text-center text-sm text-ink-faint">
        暂无内容。启动 Runtime 后，这里会显示它的 stderr 输出与协议事件。
      </p>
      <ul v-else class="space-y-0.5">
        <li
          v-for="line in lines"
          :key="line.key"
          class="flex gap-3 rounded px-2 py-1 font-mono text-xs hover:bg-surface-raised"
        >
          <span class="shrink-0 text-ink-faint">{{ formatTime(line.ts) }}</span>
          <span class="min-w-0 break-words" :class="LEVEL_CLASS[line.level]">
            {{ line.message }}
          </span>
        </li>
      </ul>
    </div>
  </Card>
</template>
