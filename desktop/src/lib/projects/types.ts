/**
 * The project column's wire shapes, mirroring `src-tauri/src/projects.rs`.
 *
 * A project is a folder the user picked, and it is also the workspace the
 * runtime is started in -- which is why the page only ever names projects by
 * the id the host handed out.
 */
export interface Project {
  /** The host's own handle on the folder: its absolute path. */
  id: string
  name: string
  path: string
  /** RFC3339, stamped when the host stored it. */
  addedAt: string
  /**
   * Whether the folder is still there. A missing project is flagged, never
   * dropped: the host does not delete what the user added.
   */
  exists: boolean
}

export interface ProjectList {
  items: Project[]
  /** The project the runtime is started in, when the user has chosen one. */
  activeId: string | null
  /** Non-fatal problems with the store, worth showing as they are. */
  warnings: string[]
}
