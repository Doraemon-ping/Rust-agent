/**
 * Departments, their members, and the group chat in front of them.
 *
 * The runtime owns these shapes: a department is a file under
 * `<workspace>/.departments`, its agents live in that file, and every task an
 * agent creates is stamped with the department id. The page passes the ids it
 * read back rather than inventing any.
 */

export type DepartmentMessageKind = 'user' | 'agent' | 'system'

/** One line in the department's group chat. */
export interface DepartmentMessage {
  id: string
  senderKind: DepartmentMessageKind
  senderId: string | null
  senderName: string
  content: string
  createdAt: string
}

/** One member of a department. */
export interface AgentProfile {
  id: string
  name: string
  role: string
  instructions: string
  lastRunAt: string | null
  lastStatus: string | null
  lastSummary: string | null
  lastCreated: number | null
  lastCompleted: number | null
}

/** How many of the department's tasks are in each state. */
export interface DepartmentTaskCounts {
  total: number
  pending: number
  inProgress: number
  completed: number
  ready: number
}

/** The shift currently driving this department, when one is running. */
export interface DepartmentShift {
  running: boolean
  startedAt: string
  currentAgentId: string | null
  agentsDone: number
  agentsTotal: number
}

/** A department, as the runtime reports it. */
export interface Department {
  id: string
  name: string
  description: string
  instructions: string
  parentId: string | null
  createdAt: string
  agents: AgentProfile[]
  messages: DepartmentMessage[]
  tasks: DepartmentTaskCounts
  shift: DepartmentShift | null
}

/** What `departments.list` answers with. */
export interface DepartmentList {
  dir: string
  count: number
  departments: Department[]
}

/** What the page may choose when it creates a department. */
export interface DepartmentInput {
  name: string
  description: string
  instructions: string
  /** The parent id, or `null` for a top-level department. */
  parentId: string | null
}

/** What the page may choose when it adds an agent. */
export interface AgentInput {
  name: string
  role: string
  instructions: string
}

export interface DepartmentChanged {
  department: Department
}

export interface DepartmentCreated {
  department: Department
}

export interface AgentCreated {
  department: Department
  agent: AgentProfile
}

export interface DepartmentRemoved {
  removed: string[]
}

export interface DepartmentRunStarted {
  accepted: boolean
  department: Department
}

export interface DepartmentStopped {
  stopped: boolean
}
