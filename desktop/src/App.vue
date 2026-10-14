<script setup lang="ts">
import { onBeforeUnmount, onMounted } from 'vue'

import AppRail from '@/components/AppRail.vue'
import ProjectPanel from '@/components/ProjectPanel.vue'
import { useRuntimeStore } from '@/stores/runtime'

const runtime = useRuntimeStore()

// The host starts the runtime during window setup; the page only subscribes.
// `connect()` is idempotent, so a hot reload cannot double-subscribe.
onMounted(() => {
  void runtime.connect()
})
onBeforeUnmount(() => runtime.disconnect())
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
