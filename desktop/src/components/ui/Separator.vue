<script setup lang="ts">
import { computed } from 'vue'

import { cn } from '@/lib/utils'

const props = withDefaults(
  defineProps<{ orientation?: 'horizontal' | 'vertical'; class?: string }>(),
  { orientation: 'horizontal' },
)

const classes = computed(() =>
  cn(
    props.orientation === 'vertical' ? 'w-px self-stretch bg-line' : 'h-px w-full bg-line',
    props.class,
  ),
)
</script>

<template>
  <div role="separator" :class="classes" />
</template>
