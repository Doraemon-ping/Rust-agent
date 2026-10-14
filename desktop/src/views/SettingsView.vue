<script setup lang="ts">
import { Bot, ListChecks, Users } from '@lucide/vue'
import { onBeforeUnmount, onMounted, ref } from 'vue'

import RuntimeActions from '@/components/RuntimeActions.vue'
import RuntimeLogPanel from '@/components/RuntimeLogPanel.vue'
import RuntimeStatusCard from '@/components/RuntimeStatusCard.vue'
import Badge from '@/components/ui/Badge.vue'
import Card from '@/components/ui/Card.vue'
import { shortenPath } from '@/lib/format'
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

/** What the workbench still owes. Listed here so the map stays honest. */
const LATER = [
  { icon: Users, name: '部门', line: '一组智能体，以及它们的协作规则' },
  { icon: Bot, name: '智能体', line: '单个智能体的角色、工具集与模型' },
  { icon: ListChecks, name: '任务', line: '认领、派发与验收' },
]
</script>

<template>
  <div class="h-full min-h-0 overflow-y-auto p-8 scroll-slim">
    <header class="mb-5">
      <p class="text-xs tracking-[0.18em] text-ink-faint uppercase">设置</p>
      <h1 class="mt-1 text-xl font-semibold tracking-tight">运行时与宿主</h1>
      <p class="mt-1 max-w-2xl text-sm text-ink-muted">
        桌面外壳启动并托管 rust-agent Runtime 子进程，通过 JSON Lines 在 stdin/stdout
        上通信。这里只展示宿主与 Runtime 真实上报的内容，不做任何推断。
      </p>
      <p class="mt-2 font-mono text-xs text-ink-faint">
        平台 {{ runtime.hostInfo?.os ?? 'unknown' }} / {{ runtime.hostInfo?.arch ?? 'unknown' }}
        <span v-if="runtime.hostInfo"> · Tauri {{ runtime.hostInfo.tauriVersion }}</span>
        <span v-if="runtime.hostInfo"> · 应用 v{{ runtime.hostInfo.appVersion }}</span>
      </p>
    </header>

    <div class="grid grid-cols-1 gap-5 xl:grid-cols-2">
      <div class="flex flex-col gap-5">
        <RuntimeStatusCard :now="now" />

        <Card>
          <template #header>
            <h2 class="text-base font-semibold tracking-tight">操作</h2>
          </template>
          <p class="mb-3 text-xs text-ink-faint">
            生命周期由宿主管理；这里的按钮只会调用宿主提供的受控接口。
          </p>
          <RuntimeActions />
        </Card>

        <Card>
          <template #header>
            <h2 class="text-base font-semibold tracking-tight">宿主</h2>
          </template>
          <dl class="space-y-3 text-xs">
            <div class="flex gap-3">
              <dt class="w-36 shrink-0 text-ink-faint">未选项目时的工作目录</dt>
              <dd
                class="min-w-0 flex-1 truncate font-mono text-ink-muted"
                :title="runtime.hostInfo?.workspace ?? ''"
              >
                {{ runtime.hostInfo ? shortenPath(runtime.hostInfo.workspace, 56) : '--' }}
              </dd>
            </div>
            <div class="flex gap-3">
              <dt class="w-36 shrink-0 text-ink-faint">Runtime 查找路径</dt>
              <dd class="min-w-0 flex-1 space-y-1">
                <p
                  v-for="candidate in runtime.hostInfo?.bridgeCandidates ?? []"
                  :key="candidate"
                  class="truncate font-mono text-ink-muted"
                  :title="candidate"
                >
                  {{ shortenPath(candidate, 56) }}
                </p>
                <p v-if="!runtime.hostInfo" class="text-ink-faint">--</p>
              </dd>
            </div>
          </dl>
        </Card>
      </div>

      <div class="flex flex-col gap-5">
        <RuntimeLogPanel class="h-[26rem]" />

        <Card>
          <template #header>
            <h2 class="text-base font-semibold tracking-tight">后续阶段</h2>
          </template>
          <ul class="space-y-2.5">
            <li v-for="item in LATER" :key="item.name" class="flex items-center gap-3 text-sm">
              <component :is="item.icon" class="size-4 shrink-0 text-ink-faint" />
              <span class="text-ink-muted">{{ item.name }}</span>
              <span class="min-w-0 flex-1 truncate text-xs text-ink-faint">{{ item.line }}</span>
              <Badge class="border-line/70 px-1.5 py-0 text-[10px]">待建</Badge>
            </li>
          </ul>
        </Card>
      </div>
    </div>
  </div>
</template>
