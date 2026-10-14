<script setup lang="ts">
import { cva, type VariantProps } from 'class-variance-authority'
import { computed } from 'vue'

import { cn } from '@/lib/utils'

const badgeVariants = cva(
  'inline-flex items-center gap-1.5 rounded-full border px-2.5 py-0.5 text-xs font-medium',
  {
    variants: {
      variant: {
        neutral: 'border-line text-ink-muted',
        ok: 'border-ok/40 bg-ok/10 text-ok',
        warn: 'border-warn/40 bg-warn/10 text-warn',
        danger: 'border-danger/45 bg-danger/10 text-danger',
        accent: 'border-accent/40 bg-accent/10 text-accent',
      },
    },
    defaultVariants: { variant: 'neutral' },
  },
)

type Variants = VariantProps<typeof badgeVariants>

const props = withDefaults(
  defineProps<{ variant?: Variants['variant']; class?: string }>(),
  { variant: 'neutral' },
)

const classes = computed(() => cn(badgeVariants({ variant: props.variant }), props.class))
</script>

<template>
  <span :class="classes">
    <slot />
  </span>
</template>
