<script setup lang="ts">
import { cn } from '@/lib/utils'

/**
 * A titled panel. The slot names follow shadcn-vue's card parts, but the whole
 * card is one component: this app never needs the pieces on their own.
 */
defineProps<{ class?: string }>()
</script>

<template>
  <section :class="cn('flex flex-col rounded-panel border border-line bg-surface', $props.class)">
    <header
      v-if="$slots.header"
      class="flex flex-wrap items-start justify-between gap-3 border-b border-line px-5 py-4"
    >
      <slot name="header" />
    </header>
    <div class="flex min-h-0 flex-1 flex-col px-5 py-4">
      <slot />
    </div>
    <footer
      v-if="$slots.footer"
      class="border-t border-line px-5 py-3 text-xs text-ink-faint"
    >
      <slot name="footer" />
    </footer>
  </section>
</template>
