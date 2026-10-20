import { defineStore } from 'pinia'
import { computed, ref, watch } from 'vue'

import { claimTask, completeTask, createTask, fetchTasks, releaseTask } from '@/lib/tasks'
import type { Task, TaskInput } from '@/lib/tasks'
import { toRuntimeError } from '@/lib/runtime/errors'
import type { RuntimeError } from '@/lib/runtime/types'
import { useRuntimeStore } from '@/stores/runtime'

/** What the page is currently asking the runtime to do. */
type Busy = '' | 'create' | string

/**
 * The task page's state.
 *
 * The board is `TaskStore`'s own `.tasks` directory, read back rather than
 * remembered: the model's task tools write the same files, so anything else
 * would be a second graph that could disagree with the one the loop works
 * from. Nothing is polled; the page asks when it is entered, when the runtime
 * comes up, and after an action it made.
 */
export const useTasksStore = defineStore('tasks', () => {
  const runtime = useRuntimeStore()
  const tasks = ref<Task[]>([])
  const dir = ref('')
  const loading = ref(false)
  const loaded = ref(false)
  const error = ref<RuntimeError | null>(null)
  /** `create`, a task id whose action is in flight, or empty. */
  const busy = ref<Busy>('')

  /** Whether the running runtime offers a board to read at all. */
  const supported = computed(() => (runtime.snapshot?.methods ?? []).includes('tasks.list'))
  /** Whether the page can read and change the board right now. */
  const ready = computed(() => runtime.ready && supported.value)
  const canChange = computed(() => ready.value && !loading.value && busy.value === '')

  const pending = computed(() => tasks.value.filter((task) => task.status === 'pending'))
  const inProgress = computed(() =>
    tasks.value.filter((task) => task.status === 'in_progress'),
  )
  const completed = computed(() =>
    tasks.value.filter((task) => task.status === 'completed'),
  )
  /** Pending nodes whose prerequisites are still open. */
  const blocked = computed(() =>
    pending.value.filter((task) => task.blocked.length > 0),
  )
  /** Pending, unowned nodes that can be claimed now. */
  const claimable = computed(() => pending.value.filter((task) => task.ready))

  /** The board in reading order: open work first, finished work last. */
  const ordered = computed(() => {
    const rank = (task: Task): number => {
      if (task.status === 'pending') return task.ready ? 0 : 1
      if (task.status === 'in_progress') return 2
      return 3
    }
    return [...tasks.value].sort((left, right) => {
      const byRank = rank(left) - rank(right)
      return byRank !== 0 ? byRank : left.id.localeCompare(right.id)
    })
  })

  /** Read the board back from the runtime. */
  async function refresh(): Promise<void> {
    if (!runtime.shell) {
      error.value = {
        code: 'not_in_shell',
        message: '当前页面不在桌面宿主中，读不到任务板。请用 `pnpm app:dev` 启动桌面外壳。',
      }
      return
    }
    if (!ready.value) return
    loading.value = true
    try {
      const answer = await fetchTasks()
      tasks.value = answer.tasks ?? []
      dir.value = answer.dir ?? ''
      error.value = null
      loaded.value = true
    } catch (failure) {
      error.value = toRuntimeError(failure)
    } finally {
      loading.value = false
    }
  }

  /** Add a task. `false` means the runtime refused it, with the reason kept. */
  async function create(input: TaskInput): Promise<boolean> {
    if (!canChange.value) return false
    busy.value = 'create'
    error.value = null
    try {
      await createTask(input)
    } catch (failure) {
      error.value = toRuntimeError(failure)
      return false
    } finally {
      busy.value = ''
    }
    await refresh()
    return true
  }

  /** Claim a ready task for the person at the page. */
  async function claim(id: string): Promise<boolean> {
    return act(id, claimTask)
  }

  /** Give a task the page claimed back to the board. */
  async function release(id: string): Promise<boolean> {
    return act(id, releaseTask)
  }

  /** Close a task the page claimed. */
  async function complete(id: string): Promise<boolean> {
    return act(id, completeTask)
  }

  async function act(id: string, run: (id: string) => Promise<unknown>): Promise<boolean> {
    if (!canChange.value) return false
    busy.value = id
    error.value = null
    try {
      await run(id)
    } catch (failure) {
      error.value = toRuntimeError(failure)
      return false
    } finally {
      busy.value = ''
    }
    await refresh()
    return true
  }

  // A runtime that just came up -- or moved to another project -- holds a
  // board of its own, so the page re-reads instead of showing the old one.
  watch(
    () => runtime.state,
    (state) => {
      if (state === 'ready') void refresh()
    },
  )

  return {
    tasks,
    dir,
    loading,
    loaded,
    error,
    busy,
    supported,
    ready,
    canChange,
    pending,
    inProgress,
    completed,
    blocked,
    claimable,
    ordered,
    refresh,
    create,
    claim,
    release,
    complete,
  }
})
