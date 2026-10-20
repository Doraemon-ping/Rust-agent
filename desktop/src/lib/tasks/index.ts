/**
 * The task board, as the page sees it.
 *
 * One of the seam modules -- alongside `lib/runtime`, `lib/chat`, `lib/cron`,
 * `lib/skills`, `lib/plugins`, `lib/projects`, and `lib/provider` -- which are
 * the only files that import `@tauri-apps/api`. The page names a subject, a
 * description, and ids the board already holds; the runtime makes the id and
 * decides whether an action is allowed.
 */

import { invoke } from '@tauri-apps/api/core'

import { requireShell } from '@/lib/shell'
import type { TaskChanged, TaskCreated, TaskInput, TaskList } from './types'

/** Every task the runtime holds, with the state its edges imply. */
export async function fetchTasks(): Promise<TaskList> {
  requireShell()
  return invoke<TaskList>('tasks_list')
}

/** Add a task, optionally behind prerequisites the page picked. */
export async function createTask(input: TaskInput): Promise<TaskCreated> {
  requireShell()
  return invoke<TaskCreated>('tasks_create', {
    subject: input.subject,
    description: input.description,
    blockedBy: input.blockedBy,
  })
}

/** Claim a ready task for the person at the page. */
export async function claimTask(id: string): Promise<TaskChanged> {
  requireShell()
  return invoke<TaskChanged>('tasks_claim', { id })
}

/** Give a task the page claimed back to the board. */
export async function releaseTask(id: string): Promise<TaskChanged> {
  requireShell()
  return invoke<TaskChanged>('tasks_release', { id })
}

/** Close a task the page claimed. */
export async function completeTask(id: string): Promise<TaskChanged> {
  requireShell()
  return invoke<TaskChanged>('tasks_complete', { id })
}

export type { Task, TaskChanged, TaskCreated, TaskInput, TaskList, TaskStatus } from './types'
