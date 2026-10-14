import { defineStore } from 'pinia'
import { computed, ref } from 'vue'

import { getProviderSettings, saveProviderSettings } from '@/lib/provider'
import type { ProviderInput, ProviderSettings } from '@/lib/provider'
import { isRunningInShell } from '@/lib/runtime'
import { toRuntimeError } from '@/lib/runtime/errors'
import type { RuntimeError } from '@/lib/runtime/types'

/**
 * The Settings page's model-provider state.
 *
 * Every value comes from the host. The form has state of its own, but this
 * store only ever reports what the host says it stored, so "saved" cannot drift
 * away from the file on disk -- and a failed write cannot leave the page
 * claiming a key it never took.
 */
export const useProviderStore = defineStore('provider', () => {
  const shell = isRunningInShell()
  const settings = ref<ProviderSettings | null>(null)
  const loaded = ref(false)
  const saving = ref(false)
  const error = ref<RuntimeError | null>(null)

  const ready = computed(() => settings.value !== null)

  /** Load the settings from the host. */
  async function refresh(): Promise<void> {
    if (!shell) {
      error.value = {
        code: 'not_in_shell',
        message:
          '当前页面不在桌面宿主中，读不到模型设置。请用 `pnpm app:dev` 启动桌面外壳。',
      }
      return
    }
    try {
      settings.value = await getProviderSettings()
      loaded.value = true
      error.value = null
    } catch (failure) {
      error.value = toRuntimeError(failure)
    }
  }

  /** Save, then adopt what the host stored. `false` means the save failed. */
  async function save(input: ProviderInput): Promise<boolean> {
    if (!shell || saving.value) return false
    saving.value = true
    try {
      settings.value = await saveProviderSettings(input)
      loaded.value = true
      error.value = null
      return true
    } catch (failure) {
      error.value = toRuntimeError(failure)
      return false
    } finally {
      saving.value = false
    }
  }

  return { shell, settings, loaded, saving, error, ready, refresh, save }
})
