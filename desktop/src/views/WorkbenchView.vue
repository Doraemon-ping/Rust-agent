<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from 'vue'

import RuntimeActions from '@/components/RuntimeActions.vue'
import RuntimeLogPanel from '@/components/RuntimeLogPanel.vue'
import RuntimeStatusCard from '@/components/RuntimeStatusCard.vue'
import { useRuntimeStore } from '@/stores/runtime'

/**
 * A one-second clock for "3 s ago" labels. It is a clock, not a poller: no
 * request is made and no state is invented, the timestamps simply age.
 */
const now = ref(Date.now())
let timer: ReturnType<typeof setInterval> | null = null

onMounted(() => {
  timer = setInterval(() => {
    now.value = Date.now()
  }, 1000)
})
onBeforeUnmount(() => {
  if (timer !== null) clearInterval(timer)
})

const runtime = useRuntimeStore()
</script>

<template>
  <div class="flex h-full min-h-0 flex-col">
    <header class="border-b border-line px-8 py-6">
      <p class="text-xs uppercase tracking-[0.18em] text-ink-faint">Workbench</p>
      <h1 class="mt-1 text-xl font-semibold tracking-tight">运行时连接</h1>
      <p class="mt-1 max-w-2xl text-sm text-ink-muted">
        桌面外壳启动并托管 rust-agent Runtime 子进程，通过 JSON Lines 在 stdin/stdout
        上通信。此处只展示 Runtime 真实上报的状态，不做任何推断。
      </p>
      <p class="mt-2 font-mono text-xs text-ink-faint">
        平台 {{ runtime.hostInfo?.os ?? 'unknown' }} / {{ runtime.hostInfo?.arch ?? 'unknown' }}
        <span v-if="runtime.hostInfo"> · Tauri {{ runtime.hostInfo.tauriVersion }}</span>
      </p>
    </header>

    <div class="grid min-h-0 flex-1 grid-cols-1 gap-5 overflow-y-auto p-8 scroll-slim xl:grid-cols-2">
      <div class="flex flex-col gap-5">
        <RuntimeStatusCard :now="now" />
        <div class="rounded-panel border border-line bg-surface px-5 py-4">
          <h2 class="text-base font-semibold tracking-tight">操作</h2>
          <p class="mt-1 mb-3 text-xs text-ink-faint">
            生命周期由宿主管理；这里的按钮只会调用宿主提供的受控接口。
          </p>
          <RuntimeActions />
        </div>
      </div>

      <RuntimeLogPanel class="min-h-[22rem]" />
    </div>
  </div>
</template>
