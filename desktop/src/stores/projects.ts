import { defineStore } from 'pinia'
import { computed, ref } from 'vue'

import {
  listProjects,
  pickProject,
  removeProject,
  selectProject,
} from '@/lib/projects'
import type { Project, ProjectList } from '@/lib/projects'
import { toRuntimeError } from '@/lib/runtime/errors'
import type { RuntimeError } from '@/lib/runtime/types'
import { isRunningInShell } from '@/lib/runtime'

/**
 * The project column's state.
 *
 * Every value comes from the host: the list, which project is active, and which
 * folders have gone missing. Nothing is cached in the browser, so the list on
 * screen is the list the runtime will actually be started in.
 */
export const useProjectsStore = defineStore('projects', () => {
  const shell = isRunningInShell()
  const items = ref<Project[]>([])
  const activeId = ref<string | null>(null)
  const warnings = ref<string[]>([])
  const busy = ref(false)
  const loaded = ref(false)
  const error = ref<RuntimeError | null>(null)

  const active = computed(
    () => items.value.find((item) => item.id === activeId.value) ?? null,
  )

  function apply(list: ProjectList): void {
    items.value = list.items
    activeId.value = list.activeId
    warnings.value = list.warnings
  }

  /** Run one host call, folding whatever it returns or throws into the state. */
  async function guarded(action: () => Promise<ProjectList | null>): Promise<void> {
    if (busy.value) return
    busy.value = true
    try {
      const list = await action()
      // A dismissed picker is not a failure: it simply changed nothing.
      if (list) apply(list)
      error.value = null
    } catch (failure) {
      error.value = toRuntimeError(failure)
    } finally {
      busy.value = false
    }
  }

  /** Load the list from the host. */
  async function refresh(): Promise<void> {
    if (!shell) {
      error.value = {
        code: 'not_in_shell',
        message:
          '当前页面不在桌面宿主中，读不到项目列表。请用 `pnpm app:dev` 启动桌面外壳。',
      }
      return
    }
    await guarded(listProjects)
    loaded.value = true
  }

  /** Open the folder picker and adopt what the user chose. */
  async function add(): Promise<void> {
    await guarded(pickProject)
  }

  /** Make a stored project active. */
  async function select(id: string): Promise<void> {
    if (id === activeId.value) return
    await guarded(() => selectProject(id))
  }

  /** Forget a project. The folder itself stays where it is. */
  async function remove(id: string): Promise<void> {
    await guarded(() => removeProject(id))
  }

  return {
    shell,
    items,
    activeId,
    warnings,
    busy,
    loaded,
    error,
    active,
    refresh,
    add,
    select,
    remove,
  }
})
