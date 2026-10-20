/**
 * The skills, as the page sees them.
 *
 * One of the seam modules -- alongside `lib/runtime`, `lib/chat`, `lib/cron`,
 * `lib/plugins`, `lib/projects`, and `lib/provider` -- which are the only files
 * that import `@tauri-apps/api`. A skill name is the one thing the page may
 * choose here; it is looked up in the runtime's own loader, and a path is never
 * something the page can name.
 */

import { invoke } from '@tauri-apps/api/core'

import { requireShell } from '@/lib/shell'
import type { SkillDetail, SkillImport, SkillList } from './types'

/**
 * Every skill the runtime found, with the manifest each one came from.
 *
 * Read-only: the directory is scanned on the other side of the pipe, so this is
 * the same catalogue the system prompt carries rather than a copy the page
 * keeps.
 */
export async function fetchSkills(): Promise<SkillList> {
  requireShell()
  return invoke<SkillList>('skills_list')
}

/** One full `SKILL.md`, the same text `load_skill` hands the model. */
export async function readSkill(name: string): Promise<SkillDetail> {
  requireShell()
  return invoke<SkillDetail>('skills_read', { name })
}

/**
 * Copy skills into the workspace from a folder the user picks.
 *
 * The host opens the picker and copies what it was given; the page never sees
 * or names a path. `null` means the dialog was dismissed, which is not a
 * failure and has nothing to report.
 */
export async function importSkills(): Promise<SkillImport | null> {
  requireShell()
  return invoke<SkillImport | null>('skills_import')
}

export type { SkillDetail, SkillImport, SkillInfo, SkillList, SkillReadInput } from './types'
