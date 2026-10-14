<script setup lang="ts">
import {
  CalendarClock,
  CircleAlert,
  CircleCheck,
  CircleDot,
  Puzzle,
  Settings,
  Sparkles,
  SquareTerminal,
} from '@lucide/vue'
import { computed } from 'vue'
import { RouterLink, useRoute } from 'vue-router'

import { RUNTIME_STATE_LABEL } from '@/lib/runtime/types'
import { useRuntimeStore } from '@/stores/runtime'

const route = useRoute()
const runtime = useRuntimeStore()

/**
 * The rail is the shell's map of itself.
 *
 * Every entry leads to a page that exists. The three that are not built yet say
 * so on the page instead of pretending here.
 */
const DESTINATIONS = [
  { name: '定时任务', to: '/cron', icon: CalendarClock },
  { name: '插件', to: '/plugins', icon: Puzzle },
  { name: '技能', to: '/skills', icon: Sparkles },
] as const

/** The runtime's health, as the host last reported it. */
const health = computed(() => {
  const label = RUNTIME_STATE_LABEL[runtime.state]
  switch (runtime.state) {
    case 'ready':
      return { icon: CircleCheck, tone: 'text-ok', title: `Runtime：${label}` }
    case 'error':
      return { icon: CircleAlert, tone: 'text-danger', title: `Runtime：${label}（详情见设置）` }
    case 'starting':
    case 'stopping':
      return { icon: CircleDot, tone: 'text-warn', title: `Runtime：${label}` }
    default:
      return { icon: CircleDot, tone: 'text-ink-faint', title: `Runtime：${label}` }
  }
})

function activeClass(path: string): string {
  return route.path === path
    ? 'bg-surface-raised text-ink'
    : 'text-ink-faint hover:bg-surface-raised hover:text-ink'
}
</script>

<template>
  <nav
    class="flex w-14 shrink-0 flex-col items-center gap-1 border-r border-line bg-surface/50 py-3"
    aria-label="主导航"
  >
    <RouterLink
      to="/"
      class="grid size-10 place-items-center rounded-xl border border-accent/40 bg-accent/10 text-accent transition hover:brightness-115"
      title="rust-agent"
      aria-label="rust-agent"
    >
      <SquareTerminal class="size-5" />
    </RouterLink>

    <div class="mt-2 flex flex-col items-center gap-1">
      <RouterLink
        v-for="item in DESTINATIONS"
        :key="item.to"
        :to="item.to"
        class="grid size-10 place-items-center rounded-lg transition-colors"
        :class="activeClass(item.to)"
        :title="item.name"
        :aria-label="item.name"
      >
        <component :is="item.icon" class="size-5" />
      </RouterLink>
    </div>

    <div class="mt-auto flex flex-col items-center gap-2">
      <span
        class="grid size-8 place-items-center rounded-lg"
        :class="health.tone"
        :title="health.title"
      >
        <component :is="health.icon" class="size-4" />
        <span class="sr-only">{{ health.title }}</span>
      </span>

      <RouterLink
        to="/settings"
        class="grid size-10 place-items-center rounded-lg transition-colors"
        :class="activeClass('/settings')"
        title="设置"
        aria-label="设置"
      >
        <Settings class="size-5" />
      </RouterLink>
    </div>
  </nav>
</template>
