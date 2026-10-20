<script setup lang="ts">
import { CircleCheck, Info, OctagonAlert, TriangleAlert } from '@lucide/vue'
import { computed } from 'vue'

import { cn } from '@/lib/utils'

type Tone = 'info' | 'warn' | 'danger' | 'ok'

const props = withDefaults(
  defineProps<{ tone?: Tone; title?: string; class?: string }>(),
  { tone: 'info' },
)

const TONE_CLASS: Record<Tone, string> = {
  info: 'border-line bg-surface-raised text-ink-muted',
  warn: 'border-warn/40 bg-warn/10 text-warn',
  danger: 'border-danger/45 bg-danger/10 text-danger',
  ok: 'border-ok/40 bg-ok/10 text-ok',
}

const TONE_ICON = {
  info: Info,
  warn: TriangleAlert,
  danger: OctagonAlert,
  ok: CircleCheck,
} as const

const classes = computed(() => cn('flex gap-3 rounded-lg border px-4 py-3 text-sm', TONE_CLASS[props.tone], props.class))
const icon = computed(() => TONE_ICON[props.tone])
</script>

<template>
  <div :class="classes" role="alert">
    <component :is="icon" class="mt-px size-4 shrink-0" />
    <div class="min-w-0 space-y-1">
      <p v-if="title" class="font-medium">{{ title }}</p>
      <slot />
    </div>
  </div>
</template>
