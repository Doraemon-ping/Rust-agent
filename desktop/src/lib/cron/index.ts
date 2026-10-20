/**
 * The schedule, as the page sees it.
 *
 * One of the seam modules -- alongside `lib/runtime`, `lib/chat`,
 * `lib/projects`, and `lib/provider` -- which are the only files that import
 * `@tauri-apps/api`. An expression and a prompt are the two things the page may
 * choose here; the job id is not, it is one the runtime handed out, and the
 * clock is the runtime's, not the page's.
 */

import { invoke } from '@tauri-apps/api/core'

import { requireShell } from '@/lib/shell'
import type { CronCancelled, CronJobInput, CronList, CronScheduled } from './types'

/**
 * Every job the runtime holds, with when each one next runs.
 *
 * The runtime owns the table the model writes to with `schedule_cron`, so this
 * is the same schedule the loop fires, not a copy the page keeps.
 */
export async function fetchCronJobs(): Promise<CronList> {
  requireShell()
  return invoke<CronList>('cron_list')
}

/**
 * Add a job to the runtime's schedule.
 *
 * The expression is validated where it is stored, so a refusal comes back with
 * the runtime's own words rather than a job that will never fire.
 */
export async function scheduleCronJob(input: CronJobInput): Promise<CronScheduled> {
  requireShell()
  return invoke<CronScheduled>('cron_schedule', { ...input })
}

/** Forget a job. The id must be one the runtime itself listed. */
export async function cancelCronJob(id: string): Promise<CronCancelled> {
  requireShell()
  return invoke<CronCancelled>('cron_cancel', { id })
}

export type { CronCancelled, CronJob, CronJobInput, CronList, CronScheduled } from './types'
