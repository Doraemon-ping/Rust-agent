import { defineStore } from 'pinia'
import { computed, ref, watch } from 'vue'

import { connectPlugin, fetchPlugins } from '@/lib/plugins'
import type { PluginServer } from '@/lib/plugins'
import { toRuntimeError } from '@/lib/runtime/errors'
import type { RuntimeError } from '@/lib/runtime/types'
import { useRuntimeStore } from '@/stores/runtime'

/**
 * The plugins page's state.
 *
 * The servers are the runtime's own registry, read back rather than remembered:
 * the page asks what is installed and shows the answer, so it can never drift
 * from the tool pool the model is given. Nothing is polled; the page asks when
 * it is entered, when the runtime comes up, and after a connect it made.
 */
export const usePluginsStore = defineStore('plugins', () => {
  const runtime = useRuntimeStore()
  const servers = ref<PluginServer[]>([])
  const connected = ref<string[]>([])
  const loading = ref(false)
  const loaded = ref(false)
  const error = ref<RuntimeError | null>(null)
  /** The server a connect is in flight for, so its own button can say so. */
  const connecting = ref('')

  /** Whether the running runtime offers a plugin list to read at all. */
  const supported = computed(() => (runtime.snapshot?.methods ?? []).includes('plugins.list'))
  /** Whether the page can read and change the plugins right now. */
  const ready = computed(() => runtime.ready && supported.value)
  const canChange = computed(() => ready.value && !loading.value && connecting.value === '')
  /** How many this build ships that the runtime has not connected. */
  const remaining = computed(() => servers.value.filter((server) => !server.connected).length)

  /** Read the plugin list back from the runtime. */
  async function refresh(): Promise<void> {
    if (!runtime.shell) {
      error.value = {
        code: 'not_in_shell',
        message: '当前页面不在桌面宿主中，读不到插件。请用 `pnpm app:dev` 启动桌面外壳。',
      }
      return
    }
    if (!ready.value) return
    loading.value = true
    try {
      const answer = await fetchPlugins()
      servers.value = answer.servers ?? []
      connected.value = answer.connected ?? []
      error.value = null
      loaded.value = true
    } catch (failure) {
      error.value = toRuntimeError(failure)
    } finally {
      loading.value = false
    }
  }

  /** Connect a plugin. `false` means the runtime refused it, with the reason kept. */
  async function connect(name: string): Promise<boolean> {
    if (!canChange.value) return false
    connecting.value = name
    error.value = null
    try {
      await connectPlugin(name)
    } catch (failure) {
      error.value = toRuntimeError(failure)
      return false
    } finally {
      connecting.value = ''
    }
    await refresh()
    return true
  }

  // A runtime that just came up -- or moved to another project -- holds a
  // registry of its own, so the page re-reads instead of showing the old one.
  watch(
    () => runtime.state,
    (state) => {
      if (state === 'ready') void refresh()
    },
  )

  return {
    servers,
    connected,
    loading,
    loaded,
    error,
    connecting,
    supported,
    ready,
    canChange,
    remaining,
    refresh,
    connect,
  }
})
