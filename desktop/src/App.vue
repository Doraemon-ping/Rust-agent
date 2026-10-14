<script setup lang="ts">
import { onBeforeUnmount, onMounted } from 'vue'

import AppSidebar from '@/components/AppSidebar.vue'
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
  <div class="flex h-full min-h-0 bg-canvas">
    <AppSidebar />
    <main class="flex min-w-0 flex-1 flex-col">
      <router-view />
    </main>
  </div>
</template>
