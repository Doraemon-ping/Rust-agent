/**
 * The skills, as the runtime reports them.
 *
 * These are the bridge's own wire shapes, passed through the host untouched,
 * which is why the fields keep the runtime's snake_case. A skill is a file the
 * runtime read: the page may name one, and everything else on these shapes is
 * something the runtime found on disk.
 */

/** One skill, as `skills.list` reports it. */
export interface SkillInfo {
  /** The name `load_skill` takes: frontmatter `name`, else the directory. */
  name: string
  /** The one-line summary the system prompt's catalogue shows. */
  description: string
  /** The `SKILL.md` this skill was read from. */
  path: string
}

/** What `skills.list` answers with: where it looked, and what it found. */
export interface SkillList {
  /** The directory the loader scanned. */
  dir: string
  count: number
  skills: SkillInfo[]
}

/** What `skills.read` answers with: one whole manifest. */
export interface SkillDetail extends SkillInfo {
  /** The full `SKILL.md`, exactly as the model receives it. */
  content: string
}

/** What the page may choose when it reads a skill. */
export interface SkillReadInput {
  name: string
}

/**
 * What `skills.import` answers with: the skills that landed.
 *
 * The count is how many folders came across in one go -- a skills repository is
 * one folder holding several skills -- and `skills` is what the loader sees
 * once they are in the workspace.
 */
export interface SkillImport {
  /** The directory they were copied into. */
  dir: string
  count: number
  skills: SkillInfo[]
}
