<script setup lang="ts">
import { cva, type VariantProps } from 'class-variance-authority'
import { computed } from 'vue'

import { cn } from '@/lib/utils'

/** shadcn-vue's button recipe, wired to this app's tokens. */
const buttonVariants = cva(
  'inline-flex items-center justify-center gap-2 whitespace-nowrap rounded-md text-sm font-medium transition-colors disabled:pointer-events-none disabled:opacity-45',
  {
    variants: {
      variant: {
        default: 'bg-accent text-accent-ink hover:brightness-115',
        outline: 'border border-line bg-transparent text-ink hover:bg-surface-raised',
        ghost: 'text-ink-muted hover:bg-surface-raised hover:text-ink',
        danger: 'border border-danger/40 text-danger hover:bg-danger/10',
      },
      size: {
        default: 'h-9 px-3.5',
        sm: 'h-8 px-3 text-xs',
        icon: 'size-9',
      },
    },
    defaultVariants: { variant: 'default', size: 'default' },
  },
)

type Variants = VariantProps<typeof buttonVariants>

const props = withDefaults(
  defineProps<{
    variant?: Variants['variant']
    size?: Variants['size']
    type?: 'button' | 'submit' | 'reset'
    disabled?: boolean
    class?: string
  }>(),
  { variant: 'default', size: 'default', type: 'button', disabled: false },
)

const classes = computed(() =>
  cn(buttonVariants({ variant: props.variant, size: props.size }), props.class),
)
</script>

<template>
  <button :type="type" :disabled="disabled" :class="classes">
    <slot />
  </button>
</template>
