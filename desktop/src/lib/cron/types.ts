/**
 * The schedule, as the runtime reports it.
 *
 * These are the bridge's own wire shapes, passed through the host untouched,
 * which is why the fields keep the runtime's snake_case. A job is the runtime's
 * record, not the page's: the page may name an expression and a prompt, and
 * everything else on this shape is something the runtime decided.
 */

/** One scheduled prompt, as `cron.list` and `cron.schedule` report it. */
export interface CronJob {
  /** `cron_` plus eight hex characters; the id the runtime handed out. */
  id: string
  /** 5-field cron expression, in the machine's local time. */
  cron: string
  /** The prompt the model is handed when the job is due. */
  prompt: string
  /** Fire on every match instead of once. */
  recurring: boolean
  /** Survive a restart of the runtime. */
  durable: boolean
  /** Due now, and waiting for the runtime to hand it to the model. */
  pending_delivery: boolean
  /** `%Y-%m-%d %H:%M` of the minute it last fired in; null until it has. */
  last_fired: string | null
  /**
   * Local wall-clock of the next run, `YYYY-MM-DDTHH:MM:SS`; null when there is
   * none. The runtime computed it from the expression and this machine's clock,
   * so it reads back as local time without a zone.
   */
  next_run: string | null
  /** Milliseconds until `next_run`, for a countdown; null when there is none. */
  next_run_ms: number | null
}

/** What `cron.list` answers with: the jobs, and how many are waiting. */
export interface CronList {
  jobs?: CronJob[]
  pending_delivery?: number
}

/** What the page may choose when it adds a job. */
export interface CronJobInput {
  cron: string
  prompt: string
  recurring: boolean
  durable: boolean
}

/** What `cron.schedule` answers with: the job the runtime stored. */
export interface CronScheduled {
  job: CronJob
}

/** What `cron.cancel` answers with: the id that went. */
export interface CronCancelled {
  cancelled: string
}
