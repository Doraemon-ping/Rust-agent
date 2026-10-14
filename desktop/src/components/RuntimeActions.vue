<script setup lang="ts">
import { PlugZap, Play, RefreshCw, Square } from '@lucide/vue'
import { computed } from 'vue'

import Badge from '@/components/ui/Badge.vue'
import Button from '@/components/ui/Button.vue'
import { formatTime } from '@/lib/format'
import { useRuntimeStore } from '@/stores/runtime'

const runtime = useRuntimeStore()

const canStart = computed(
  () => runtime.stopped || runtime.state === 'error',
)
const canStop = computed(() => runtime.state === 'ready' || runtime.state === 'starting')
const canPing = computed(() => runtime.state === 'ready' && !runtime.busy)
const canReconnect = computed(() => runtime.shell && !runtime.busy)

const pingLabel = computed(() => {
  const ping = runtime.status.lastPing
  if (!ping) return null
  if (ping.ok) {
    return `最近一次连接检查成功 · ${ping.latencyMs} ms · ${formatTime(ping.at)}`
  }
  const reason = ping.error ? `：${ping.error.message}` : ''
  return `最近一次连接检查失败${reason} · ${formatTime(ping.at)}`
})
</script>

<template>
  <div class="space-y-3">
    <div class="flex flex-wrap gap-2">
      <Button
        variant="outline"
        :disabled="!canPing"
        @click="runtime.ping()"
      >
        <PlugZap class="size-4" />
        连接检查
      </Button>
      <Button
        variant="outline"
        :disabled="!canReconnect"
        @click="runtime.restart()"
      >
        <RefreshCw class="size-4" :class="runtime.busy ? 'animate-spin' : ''" />
        重新连接
      </Button>
      <Button v-if="canStart" :disabled="!runtime.shell || runtime.busy" @click="runtime.start()">
        <Play class="size-4" />
        启动 Runtime
      </Button>
      <Button
        variant="danger"
        :disabled="!canStop || runtime.busy"
        @click="runtime.stop()"
      >
        <Square class="size-4" />
        停止 Runtime
      </Button>
    </div>

    <div class="flex flex-wrap items-center gap-2 text-xs text-ink-faint">
      <Badge v-if="runtime.busy" variant="warn">处理中…</Badge>
      <span v-if="pingLabel">{{ pingLabel }}</span>
      <span v-else>尚未执行连接检查</span>
    </div>

    <p v-if="!runtime.shell" class="text-xs text-warn">
      当前页面不在桌面宿主中，控制按钮已禁用。
    </p>
  </div>
</template>
