import { defineStore } from 'pinia'
import { computed, ref, watch } from 'vue'

import { cancelCronJob, fetchCronJobs, scheduleCronJob } from '@/lib/cron'
import type { CronJob, CronJobInput } from '@/lib/cron'
import { toRuntimeError } from '@/lib/runtime/errors'
import type { RuntimeError } from '@/lib/runtime/types'
import { useRuntimeStore } from '@/stores/runtime'

/**
 * The schedule page's state.
 *
 * The jobs are the runtime's own, read back rather than remembered: the page
 * asks what is scheduled and shows the answer, so it can never drift from the
 * table the loop fires. Nothing is polled on a timer; the page asks again when
 * it is entered, when the runtime comes up, after a change it made, and when a
 * next run it was showing has passed.
 */
export const useCronStore = defineStore('cron', () => {
  const runtime = useRuntimeStore()
  const jobs = ref<CronJob[]>([])
  const loading = ref(false)
  const loaded = ref(false)
  const error = ref<RuntimeError | null>(null)
  /** When the list on screen was read, in `Date.now()` terms. */
  const readAt = ref(0)
  /** Whether the page has already asked again for the deadline it is showing. */
  let askedFor = 0

  /** Whether the running runtime offers a schedule to read at all. */
  const supported = computed(() => (runtime.snapshot?.methods ?? []).includes('cron.list'))
  /** Whether the page can read and change the schedule right now. */
  const ready = computed(() => runtime.ready && supported.value)
  const canChange = computed(() => ready.value && !loading.value)
  /** Jobs that are due and still waiting for the runtime to deliver them. */
  const waiting = computed(() => jobs.value.filter((job) => job.pending_delivery).length)

  /** The soonest next run still ahead, in milliseconds from `readAt`. */
  const nextDeadline = computed(() => {
    const deadlines = jobs.value
      .map((job) => job.next_run_ms)
      .filter((ms): ms is number => ms !== null)
    return deadlines.length ? Math.min(...deadlines) : null
  })

  /** Read the schedule back from the runtime. */
  async function refresh(): Promise<void> {
    if (!runtime.shell) {
      error.value = {
        code: 'not_in_shell',
        message: '当前页面不在桌面宿主中，读不到定时任务。请用 `pnpm app:dev` 启动桌面外壳。',
      }
      return
    }
    if (!ready.value) return
    loading.value = true
    try {
      const answer = await fetchCronJobs()
      jobs.value = answer.jobs ?? []
      readAt.value = Date.now()
      askedFor = 0
      error.value = null
      loaded.value = true
    } catch (failure) {
      error.value = toRuntimeError(failure)
    } finally {
      loading.value = false
    }
  }

  /** Add a job. `false` means the runtime refused it, with the reason kept. */
  async function schedule(input: CronJobInput): Promise<boolean> {
    if (!canChange.value) return false
    error.value = null
    try {
      await scheduleCronJob(input)
    } catch (failure) {
      error.value = toRuntimeError(failure)
      return false
    }
    await refresh()
    return true
  }

  /** Forget a job. `false` means the runtime did not take the id. */
  async function cancel(id: string): Promise<boolean> {
    if (!canChange.value) return false
    error.value = null
    try {
      await cancelCronJob(id)
    } catch (failure) {
      error.value = toRuntimeError(failure)
      return false
    }
    await refresh()
    return true
  }

  /**
   * Ask again when a next run the page was showing has passed.
   *
   * A job that fired has moved on -- it is waiting to be delivered, or its
   * next match is later -- and the countdown on screen would otherwise sit at
   * zero. This is one request per passed deadline, not a poll: `clock` is the
   * page's own second hand, and nothing is asked for while nothing is due.
   */
  function refreshIfDue(now: number): void {
    const deadline = nextDeadline.value
    if (deadline === null || !ready.value || loading.value) return
    if (now - readAt.value < deadline + 1000) return
    // Once per deadline, so a job that stays due does not re-ask every second.
    const stamp = readAt.value + deadline
    if (askedFor === stamp) return
    askedFor = stamp
    void refresh()
  }

  // A runtime that just came up -- or moved to another project -- holds a
  // schedule of its own, so the page re-reads instead of showing the old one.
  watch(
    () => runtime.state,
    (state) => {
      if (state === 'ready') void refresh()
    },
  )

  return {
    jobs,
    loading,
    loaded,
    error,
    readAt,
    supported,
    ready,
    canChange,
    waiting,
    refresh,
    schedule,
    cancel,
    refreshIfDue,
  }
})
