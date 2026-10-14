<script setup lang="ts">
import { onBeforeUnmount, onMounted } from 'vue'

import AppRail from '@/components/AppRail.vue'
import ProjectPanel from '@/components/ProjectPanel.vue'
import { useChatStore } from '@/stores/chat'
import { useRuntimeStore } from '@/stores/runtime'

const runtime = useRuntimeStore()
const chat = useChatStore()

// The host starts the runtime during window setup; the page only subscribes.
// `connect()` is idempotent, so a hot reload cannot double-subscribe.
//
// The chat subscribes here rather than on its own page, so a turn that is still
// running while the user is elsewhere in the shell is not lost.
onMounted(() => {
  void runtime.connect()
  void chat.connect()
})
onBeforeUnmount(() => {
  runtime.disconnect()
  chat.disconnect()
})
</script>

<template>
  <!-- Three columns, the shell's whole shape: destinations, projects, work. -->
  <div class="flex h-full min-h-0 bg-canvas">
    <AppRail />
    <ProjectPanel />
    <main class="flex min-w-0 flex-1 flex-col">
      <router-view />
    </main>
  </div>
</template>
