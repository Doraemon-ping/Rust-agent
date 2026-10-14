<script setup lang="ts">
import { Puzzle } from '@lucide/vue'

import PlaceholderView from '@/views/PlaceholderView.vue'
</script>

<template>
  <PlaceholderView
    title="插件"
    blurb="给 runtime 装上外部能力。协议里还没有插件这一项，所以这一页现在连事实都报不出来。"
    :icon="Puzzle"
    :pending="[
      '列出 runtime 加载的插件与它们提供的能力',
      '按项目启用或停用某个插件',
      '显示插件加载失败的原因',
    ]"
  />
</template>
