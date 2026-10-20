import { defineStore } from 'pinia'
import { computed, ref, watch } from 'vue'

import { fetchSkills, importSkills, readSkill } from '@/lib/skills'
import type { SkillDetail, SkillInfo } from '@/lib/skills'
import { toRuntimeError } from '@/lib/runtime/errors'
import type { RuntimeError } from '@/lib/runtime/types'
import { useRuntimeStore } from '@/stores/runtime'

/**
 * The skills page's state.
 *
 * The list is the runtime's own: the page asks what the loader found instead of
 * remembering, so it can never drift from the catalogue the system prompt
 * carries. Nothing is polled; the page asks when it is entered and when the
 * runtime comes up. One manifest is read on demand and kept only while the
 * page is showing it.
 */
export const useSkillsStore = defineStore('skills', () => {
  const runtime = useRuntimeStore()
  const skills = ref<SkillInfo[]>([])
  /** Where the runtime looked, as it reported it. */
  const dir = ref('')
  const loading = ref(false)
  const loaded = ref(false)
  const error = ref<RuntimeError | null>(null)
  /** The skill the detail column is showing, or the empty name for none. */
  const selected = ref('')
  /** That skill's whole manifest, once the runtime has handed it over. */
  const detail = ref<SkillDetail | null>(null)
  const reading = ref(false)
  /** True while the folder picker is open or its answer is being copied. */
  const importing = ref(false)
  /** How many skills the last import landed, for the page to say so. */
  const imported = ref(0)

  /** Whether the running runtime offers a catalogue to read at all. */
  const supported = computed(() => (runtime.snapshot?.methods ?? []).includes('skills.list'))
  /** Whether the page can read the catalogue right now. */
  const ready = computed(() => runtime.ready && supported.value)
  const canRead = computed(() => ready.value && !reading.value)
  /** Whether the runtime can take skills from a folder the user picks. */
  const importSupported = computed(() =>
    (runtime.snapshot?.methods ?? []).includes('skills.import'),
  )
  const canImport = computed(() => ready.value && importSupported.value && !importing.value)
  /** The listed skill behind `selected`, for its description and path. */
  const chosen = computed(
    () => skills.value.find((skill) => skill.name === selected.value) ?? null,
  )

  /** Read the catalogue back from the runtime. */
  async function refresh(): Promise<void> {
    if (!runtime.shell) {
      error.value = {
        code: 'not_in_shell',
        message: '当前页面不在桌面宿主中，读不到技能。请用 `pnpm app:dev` 启动桌面外壳。',
      }
      return
    }
    if (!ready.value) return
    loading.value = true
    try {
      const answer = await fetchSkills()
      skills.value = answer.skills ?? []
      dir.value = answer.dir ?? ''
      error.value = null
      loaded.value = true
      // A skill that is gone takes its manifest with it, rather than leaving
      // the detail column showing a file that is no longer there.
      if (selected.value && !skills.value.some((skill) => skill.name === selected.value)) {
        selected.value = ''
        detail.value = null
      }
    } catch (failure) {
      error.value = toRuntimeError(failure)
    } finally {
      loading.value = false
    }
  }

  /**
   * Read one skill's manifest into the detail column.
   *
   * Choosing the open skill again puts it away, so the column has a close that
   * does not need a second control. `false` means the runtime refused to hand
   * the manifest over, with the reason kept.
   */
  async function open(name: string): Promise<boolean> {
    if (!canRead.value) return false
    if (selected.value === name && detail.value !== null) {
      selected.value = ''
      detail.value = null
      return true
    }
    selected.value = name
    detail.value = null
    reading.value = true
    error.value = null
    try {
      detail.value = await readSkill(name)
      return true
    } catch (failure) {
      error.value = toRuntimeError(failure)
      return false
    } finally {
      reading.value = false
    }
  }

  /** Put the manifest away without reading another one. */
  function close(): void {
    selected.value = ''
    detail.value = null
  }

  /**
   * Copy skills in from a folder the user picks.
   *
   * The host opens the picker and the runtime does the copying; the page is
   * told what landed and re-reads the catalogue, so what it shows is what the
   * loader now holds. A dismissed dialog lands nothing and says nothing.
   */
  async function importFrom(): Promise<void> {
    if (!canImport.value) return
    importing.value = true
    error.value = null
    imported.value = 0
    try {
      const answer = await importSkills()
      if (!answer) return
      imported.value = answer.count
      await refresh()
    } catch (failure) {
      error.value = toRuntimeError(failure)
    } finally {
      importing.value = false
    }
  }

  // A runtime that just came up -- or moved to another project -- holds a
  // catalogue of its own, so the page re-reads instead of showing the old one.
  watch(
    () => runtime.state,
    (state) => {
      if (state === 'ready') void refresh()
    },
  )

  return {
    skills,
    dir,
    loading,
    loaded,
    error,
    selected,
    detail,
    reading,
    importing,
    imported,
    supported,
    ready,
    canRead,
    importSupported,
    canImport,
    chosen,
    refresh,
    importFrom,
    open,
    close,
  }
})
