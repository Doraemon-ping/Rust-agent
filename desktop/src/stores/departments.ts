import { defineStore } from 'pinia'
import { computed, ref, watch } from 'vue'

import {
  addAgent,
  createDepartment,
  fetchDepartments,
  removeAgent,
  removeDepartment,
  runDepartment,
  stopDepartment,
} from '@/lib/departments'
import type { AgentInput, Department, DepartmentInput } from '@/lib/departments'
import { toRuntimeError } from '@/lib/runtime/errors'
import type { RuntimeError } from '@/lib/runtime/types'
import { useRuntimeStore } from '@/stores/runtime'

interface DepartmentRow {
  department: Department
  depth: number
}

/**
 * The departments page's state.
 *
 * The runtime owns departments, agents, the group chat, and the shift. This
 * store reads them back rather than keeping a second copy: while a shift is
 * running it refreshes on the runtime's `department.*` events, with a slow
 * timer as the fallback for a page that missed one.
 */
export const useDepartmentsStore = defineStore('departments', () => {
  const runtime = useRuntimeStore()
  const departments = ref<Department[]>([])
  const dir = ref('')
  const loading = ref(false)
  const loaded = ref(false)
  const error = ref<RuntimeError | null>(null)
  const busy = ref('')
  const selectedId = ref('')
  let poller: ReturnType<typeof setInterval> | null = null

  const supported = computed(() =>
    (runtime.snapshot?.methods ?? []).includes('departments.list'),
  )
  const ready = computed(() => runtime.ready && supported.value)
  const canChange = computed(() => ready.value && !loading.value && busy.value === '')
  const selected = computed(
    () => departments.value.find((department) => department.id === selectedId.value) ?? null,
  )
  const roots = computed(() => departments.value.filter((department) => !department.parentId))
  /** The department tree flattened in display order, with a nesting depth. */
  const rows = computed<DepartmentRow[]>(() => {
    const children = new Map<string, Department[]>()
    const top: Department[] = []
    for (const department of departments.value) {
      if (!department.parentId) {
        top.push(department)
        continue
      }
      const siblings = children.get(department.parentId) ?? []
      siblings.push(department)
      children.set(department.parentId, siblings)
    }
    const byName = (left: Department, right: Department): number =>
      left.name.localeCompare(right.name, 'zh-CN')
    const walk = (items: Department[], depth: number, out: DepartmentRow[]): void => {
      for (const department of [...items].sort(byName)) {
        out.push({ department, depth })
        walk(children.get(department.id) ?? [], depth + 1, out)
      }
    }
    const out: DepartmentRow[] = []
    walk(top, 0, out)
    return out
  })
  const running = computed(() => departments.value.some((department) => department.shift))
  const totalAgents = computed(() =>
    departments.value.reduce((count, department) => count + department.agents.length, 0),
  )

  function stopPolling(): void {
    if (poller !== null) {
      clearInterval(poller)
      poller = null
    }
  }

  function syncPolling(): void {
    if (runtime.ready && running.value && poller === null) {
      poller = setInterval(() => void refresh(), 1500)
    } else if ((!runtime.ready || !running.value) && poller !== null) {
      stopPolling()
    }
  }

  /** Read departments back from the runtime. */
  async function refresh(): Promise<void> {
    if (!runtime.shell) {
      error.value = {
        code: 'not_in_shell',
        message: '当前页面不在桌面宿主中，读不到部门。请用 `pnpm app:dev` 启动桌面外壳。',
      }
      return
    }
    if (!ready.value || loading.value) return
    loading.value = true
    try {
      const answer = await fetchDepartments()
      departments.value = answer.departments ?? []
      dir.value = answer.dir ?? ''
      loaded.value = true
      error.value = null
      if (!departments.value.some((department) => department.id === selectedId.value)) {
        selectedId.value = roots.value[0]?.id ?? departments.value[0]?.id ?? ''
      }
    } catch (failure) {
      error.value = toRuntimeError(failure)
    } finally {
      loading.value = false
      syncPolling()
    }
  }

  function select(id: string): void {
    selectedId.value = id
  }

  async function create(input: DepartmentInput): Promise<boolean> {
    if (!canChange.value) return false
    busy.value = 'department'
    error.value = null
    try {
      const answer = await createDepartment(input)
      selectedId.value = answer.department.id
    } catch (failure) {
      error.value = toRuntimeError(failure)
      return false
    } finally {
      busy.value = ''
    }
    await refresh()
    return true
  }

  async function remove(id: string): Promise<boolean> {
    if (!canChange.value) return false
    busy.value = id
    error.value = null
    try {
      await removeDepartment(id)
    } catch (failure) {
      error.value = toRuntimeError(failure)
      return false
    } finally {
      busy.value = ''
    }
    await refresh()
    return true
  }

  async function addMember(departmentId: string, input: AgentInput): Promise<boolean> {
    if (!canChange.value) return false
    busy.value = `agent:${departmentId}`
    error.value = null
    try {
      await addAgent(departmentId, input)
    } catch (failure) {
      error.value = toRuntimeError(failure)
      return false
    } finally {
      busy.value = ''
    }
    await refresh()
    return true
  }

  async function removeMember(departmentId: string, agentId: string): Promise<boolean> {
    if (!canChange.value) return false
    busy.value = `agent:${departmentId}`
    error.value = null
    try {
      await removeAgent(departmentId, agentId)
    } catch (failure) {
      error.value = toRuntimeError(failure)
      return false
    } finally {
      busy.value = ''
    }
    await refresh()
    return true
  }

  async function run(id: string, instruction: string): Promise<boolean> {
    if (!canChange.value) return false
    busy.value = `run:${id}`
    error.value = null
    try {
      await runDepartment(id, instruction)
    } catch (failure) {
      error.value = toRuntimeError(failure)
      return false
    } finally {
      busy.value = ''
    }
    await refresh()
    return true
  }

  async function stop(id: string): Promise<boolean> {
    if (!ready.value || busy.value !== '') return false
    busy.value = `stop:${id}`
    error.value = null
    try {
      await stopDepartment(id)
    } catch (failure) {
      error.value = toRuntimeError(failure)
      return false
    } finally {
      busy.value = ''
    }
    await refresh()
    return true
  }

  watch(
    () => runtime.state,
    (state) => {
      if (state === 'ready') void refresh()
      else stopPolling()
    },
  )

  watch(
    () => runtime.events.at(-1),
    (event) => {
      if (event?.event.startsWith('department.')) void refresh()
    },
  )

  return {
    departments,
    dir,
    loading,
    loaded,
    error,
    busy,
    selectedId,
    supported,
    ready,
    canChange,
    selected,
    roots,
    rows,
    running,
    totalAgents,
    refresh,
    select,
    create,
    remove,
    addMember,
    removeMember,
    run,
    stop,
  }
})
