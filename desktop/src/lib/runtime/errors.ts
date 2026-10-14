import type { RuntimeError } from './types'

/**
 * Tauri rejects an `invoke` with whatever the command returned as `Err`, which
 * for our host commands is a `{ code, message }` object. Anything else -- a
 * string, a thrown `Error`, a rejected fetch -- is normalised here so the UI
 * always has a code and a sentence to show.
 */
export function toRuntimeError(error: unknown): RuntimeError {
  if (typeof error === 'string' && error.trim() !== '') {
    return { code: 'host_error', message: error }
  }

  if (error && typeof error === 'object') {
    const candidate = error as { code?: unknown; message?: unknown }
    if (typeof candidate.code === 'string' && typeof candidate.message === 'string') {
      return { code: candidate.code, message: candidate.message }
    }
    if (typeof candidate.message === 'string') {
      return { code: 'host_error', message: candidate.message }
    }
  }

  if (error instanceof Error) {
    return { code: 'frontend_error', message: error.message }
  }

  return { code: 'unknown_error', message: String(error) }
}
