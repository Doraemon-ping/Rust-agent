/**
 * The task board, as the runtime reports it.
 *
 * A task is a file under `<workspace>/.tasks`, and the board is the same graph
 * the model's `create_task` / `claim_task` / `complete_task` tools write to.
 * These are the bridge's wire shapes, passed through the host untouched, which
 * is why the persisted fields keep the runtime's names.
 */

/** Where a task stands. */
export type TaskStatus = 'pending' | 'in_progress' | 'completed'

/** One durable task, with the state its dependency edges imply. */
export interface Task {
  /** `task_` plus eight hex characters; the id the runtime handed out. */
  id: string
  /** Short title. */
  subject: string
  /** Longer description; empty when the task was created without one. */
  description: string
  status: TaskStatus
  /** Who claimed it: `user` from this page, `agent` or a teammate from a turn. */
  owner: string | null
  /** The ids this task waits on, as the store spells them on disk. */
  blockedBy: string[]
  /** A teammate worktree bound to the task, when there is one. */
  worktree: string | null
  /** The prerequisites that are not complete yet; empty means ready to start. */
  blocked: string[]
  /** Pending, unowned, and every prerequisite is complete. */
  ready: boolean
}

/** What `tasks.list` answers with: where the board lives, and every node. */
export interface TaskList {
  /** The `.tasks` directory the runtime read. */
  dir: string
  count: number
  tasks: Task[]
}

/** What the page may choose when it adds a task. */
export interface TaskInput {
  subject: string
  description: string
  /** Ids the page read from the board; the runtime checks each one. */
  blockedBy: string[]
}

/** What `tasks.create` answers with: the node the runtime stored. */
export interface TaskCreated {
  task: Task
}

/** What a claim, release, or completion answers with. */
export interface TaskChanged {
  task: Task
  /** The runtime's own sentence, for a claim or completion. */
  message?: string
}
