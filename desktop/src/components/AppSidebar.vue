<script setup lang="ts">
import {
  Activity,
  CircleAlert,
  CircleCheck,
  ListChecks,
  Settings2,
  SquareTerminal,
  Users,
  Workflow,
} from '@lucide/vue'
import { computed } from 'vue'
import { RouterLink, useRoute } from 'vue-router'

import Badge from '@/components/ui/Badge.vue'
import { RUNTIME_STATE_LABEL } from '@/lib/runtime/types'
import { useRuntimeStore } from '@/stores/runtime'

const runtime = useRuntimeStore()
const route = useRoute()

/**
 * Navigation for the workbench. Only the workbench exists in phase 1; the rest
 * are listed and marked, so the shell shows its shape without pretending the
 * features are there.
 */
const NAV = [
  { name: '工作台', icon: SquareTerminal, to: '/', ready: true },
  { name: '部门', icon: Users, to: '/departments', ready: false },
  { name: '智能体', icon: Settings2, to: '/agents', ready: false },
  { name: '任务', icon: ListChecks, to: '/tasks', ready: false },
  { name: '技能与工作流', icon: Workflow, to: '/skills', ready: false },
  { name: '运行日志', icon: Activity, to: '/logs', ready: false },
]

const stateTone = computed(() => {
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

const appVersion = computed(() => runtime.hostInfo?.appVersion ?? '0.1.0')
const protocolVersion = computed(
  () => runtime.status.protocolVersion ?? runtime.hostInfo?.protocolVersion ?? null,
)
</script>

<template>
  <aside
    class="flex w-64 shrink-0 flex-col border-r border-line bg-surface/60"
    aria-label="Primary"
  >
    <div class="flex items-center gap-3 px-5 py-5">
      <div
        class="grid size-9 shrink-0 place-items-center rounded-xl border border-accent/40 bg-accent/10 text-accent"
      >
        <SquareTerminal class="size-4.5" />
      </div>
      <div class="min-w-0">
        <p class="truncate font-semibold tracking-tight">rust-agent</p>
        <p class="truncate text-xs text-ink-faint">协作工作台</p>
      </div>
    </div>

    <nav class="flex-1 space-y-1 overflow-y-auto px-3 pb-4 scroll-slim">
      <template v-for="item in NAV" :key="item.name">
        <RouterLink
          v-if="item.ready"
          :to="item.to"
          class="flex items-center gap-3 rounded-lg px-3 py-2 text-sm transition-colors"
          :class="
            route.path === item.to
              ? 'bg-surface-raised text-ink'
              : 'text-ink-muted hover:bg-surface-raised hover:text-ink'
          "
        >
          <component :is="item.icon" class="size-4 shrink-0" />
          <span class="truncate">{{ item.name }}</span>
        </RouterLink>
        <div
          v-else
          class="flex items-center gap-3 rounded-lg px-3 py-2 text-sm text-ink-faint/70"
          :title="`${item.name}：后续阶段`"
        >
          <component :is="item.icon" class="size-4 shrink-0" />
          <span class="truncate">{{ item.name }}</span>
          <Badge class="ml-auto border-line/70 px-1.5 py-0 text-[10px]">待建</Badge>
        </div>
      </template>
    </nav>

    <div class="space-y-3 border-t border-line px-5 py-4">
      <div class="flex items-center gap-2 text-xs">
        <CircleCheck v-if="runtime.state === 'ready'" class="size-3.5 text-ok" />
        <CircleAlert
          v-else-if="runtime.state === 'error'"
          class="size-3.5 text-danger"
        />
        <Activity v-else class="size-3.5 text-ink-faint" />
        <span class="text-ink-muted">Runtime</span>
        <Badge :variant="stateTone" class="ml-auto">
          {{ RUNTIME_STATE_LABEL[runtime.state] }}
        </Badge>
      </div>
      <dl class="space-y-1 text-xs text-ink-faint">
        <div class="flex justify-between gap-2">
          <dt>应用版本</dt>
          <dd class="font-mono text-ink-muted">v{{ appVersion }}</dd>
        </div>
        <div class="flex justify-between gap-2">
          <dt>协议版本</dt>
          <dd class="font-mono text-ink-muted">
            {{ protocolVersion === null ? '--' : `v${protocolVersion}` }}
          </dd>
        </div>
      </dl>
    </div>
  </aside>
</template>
