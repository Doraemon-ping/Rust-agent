/**
 * The project list, as the page sees it.
 *
 * This is the second of the two seam modules (the other is `lib/runtime`): they
 * are the only files that import `@tauri-apps/api`. The page can ask the host
 * to open the folder picker and can choose among the projects the host already
 * stored -- it never hands the host a path of its own.
 */

import { invoke } from '@tauri-apps/api/core'

import { requireShell } from '@/lib/shell'
import type { ProjectList } from './types'

/** The stored projects, which one is active, and any warning about the store. */
export async function listProjects(): Promise<ProjectList> {
  requireShell()
  return invoke<ProjectList>('projects_list')
}

/** Open the system folder picker and add what the user chose. `null` = dismissed. */
export async function pickProject(): Promise<ProjectList | null> {
  requireShell()
  return invoke<ProjectList | null>('projects_pick')
}

/** Make a stored project active. A live runtime is restarted into it. */
export async function selectProject(id: string): Promise<ProjectList> {
  requireShell()
  return invoke<ProjectList>('projects_select', { id })
}

/** Forget a project. The folder on disk is not touched. */
export async function removeProject(id: string): Promise<ProjectList> {
  requireShell()
  return invoke<ProjectList>('projects_remove', { id })
}

export type { Project, ProjectList } from './types'
