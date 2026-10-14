<script setup lang="ts">
import { CalendarClock } from '@lucide/vue'
import { computed } from 'vue'

import { useRuntimeStore } from '@/stores/runtime'
import PlaceholderView from '@/views/PlaceholderView.vue'

const runtime = useRuntimeStore()

/** Only what the runtime itself reports; nothing is counted here. */
const facts = computed(() => {
  const snapshot = runtime.snapshot
  if (!snapshot) return []
  return [
    { label: '定时任务', value: `${snapshot.cron.jobs}` },
    { label: '待投递', value: `${snapshot.cron.pending_delivery}` },
  ]
})
</script>

<template>
  <PlaceholderView
    title="定时任务"
    blurb="让 runtime 按时间自己跑起来。页面还没建，但 runtime 已经在报告它自己的定时任务数量。"
    :icon="CalendarClock"
    :facts="facts"
    :pending="[
      '列出每个定时任务的下一次运行时间与状态',
      '新建、暂停、立即执行一个任务',
      '把每一次投递的结果显示出来',
    ]"
  />
</template>
