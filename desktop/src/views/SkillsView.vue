<script setup lang="ts">
import { Sparkles } from '@lucide/vue'
import { computed } from 'vue'

import { useRuntimeStore } from '@/stores/runtime'
import PlaceholderView from '@/views/PlaceholderView.vue'

const runtime = useRuntimeStore()

/** The runtime loads skills from `<workspace>/skills` and reports them. */
const facts = computed(() => {
  const snapshot = runtime.snapshot
  if (!snapshot) return []
  return [
    { label: '技能数量', value: `${snapshot.skills.count}` },
    {
      label: '技能名',
      value: snapshot.skills.names.length ? snapshot.skills.names.join('、') : '（无）',
    },
  ]
})
</script>

<template>
  <PlaceholderView
    title="技能"
    blurb="runtime 会从工作目录的 skills/ 里加载技能。它已经在报告加载了几条，页面还没建。"
    :icon="Sparkles"
    :facts="facts"
    :pending="[
      '列出每个技能的来源目录与描述',
      '按项目开关某条技能',
      '显示每条技能最近一次被使用的时间',
    ]"
  />
</template>
