/**
 * The departments seam.
 *
 * As with the other feature seams, this is the only module that talks to the
 * host about departments. The page names text and ids it read back; the
 * runtime decides the rest.
 */

import { invoke } from '@tauri-apps/api/core'

import { requireShell } from '@/lib/shell'
import type {
  AgentCreated,
  AgentInput,
  DepartmentCreated,
  DepartmentInput,
  DepartmentList,
  DepartmentRemoved,
  DepartmentRunStarted,
  DepartmentStopped,
} from './types'

/** Every department, with its roster, group chat, task counts, and shift. */
export async function fetchDepartments(): Promise<DepartmentList> {
  requireShell()
  return invoke<DepartmentList>('departments_list')
}

/** Create a department or a sub-department under `parentId`. */
export async function createDepartment(input: DepartmentInput): Promise<DepartmentCreated> {
  requireShell()
  return invoke<DepartmentCreated>('departments_create', {
    name: input.name,
    description: input.description,
    instructions: input.instructions,
    parentId: input.parentId,
  })
}

/** Remove a department and every sub-department below it. */
export async function removeDepartment(id: string): Promise<DepartmentRemoved> {
  requireShell()
  return invoke<DepartmentRemoved>('departments_remove', { id })
}

/** Add one agent to a department. */
export async function addAgent(
  departmentId: string,
  input: AgentInput,
): Promise<AgentCreated> {
  requireShell()
  return invoke<AgentCreated>('agents_add', {
    departmentId,
    name: input.name,
    role: input.role,
    instructions: input.instructions,
  })
}

/** Remove one agent from a department. */
export async function removeAgent(
  departmentId: string,
  agentId: string,
): Promise<{ department: DepartmentCreated['department'] }> {
  requireShell()
  return invoke<{ department: DepartmentCreated['department'] }>('agents_remove', {
    departmentId,
    agentId,
  })
}

/** Send the department to work. The call returns before the agents finish. */
export async function runDepartment(
  id: string,
  instruction: string,
): Promise<DepartmentRunStarted> {
  requireShell()
  return invoke<DepartmentRunStarted>('departments_run', { id, instruction })
}

/** Ask the running shift to stop. */
export async function stopDepartment(id: string): Promise<DepartmentStopped> {
  requireShell()
  return invoke<DepartmentStopped>('departments_stop', { id })
}

export type {
  AgentCreated,
  AgentInput,
  AgentProfile,
  Department,
  DepartmentChanged,
  DepartmentCreated,
  DepartmentInput,
  DepartmentList,
  DepartmentMessage,
  DepartmentMessageKind,
  DepartmentRemoved,
  DepartmentRunStarted,
  DepartmentShift,
  DepartmentStopped,
  DepartmentTaskCounts,
} from './types'
