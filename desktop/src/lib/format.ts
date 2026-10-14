/** Display helpers. Pure functions, so they are easy to reason about. */

/** `1.4 s`, `12 s`, `3 m 04 s`, `2 h 11 m`. */
export function formatDuration(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return '--'
  const seconds = Math.floor(ms / 1000)
  if (seconds < 1) return '<1 s'
  if (seconds < 60) return `${seconds} s`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes} m ${String(seconds % 60).padStart(2, '0')} s`
  const hours = Math.floor(minutes / 60)
  return `${hours} h ${String(minutes % 60).padStart(2, '0')} m`
}

/** Local wall-clock time for a wire timestamp. */
export function formatTime(iso: string | null): string {
  if (!iso) return '--'
  const date = new Date(iso)
  if (Number.isNaN(date.getTime())) return iso
  return date.toLocaleTimeString(undefined, { hour12: false })
}

/** `just now`, `4 s ago`, `3 min ago`, `2 h ago`. */
export function formatRelative(iso: string | null, now: number = Date.now()): string {
  if (!iso) return '--'
  const then = new Date(iso).getTime()
  if (Number.isNaN(then)) return iso
  const delta = Math.max(0, now - then)
  if (delta < 2000) return 'just now'
  if (delta < 60_000) return `${Math.floor(delta / 1000)} s ago`
  if (delta < 3_600_000) return `${Math.floor(delta / 60_000)} min ago`
  if (delta < 86_400_000) return `${Math.floor(delta / 3_600_000)} h ago`
  return `${Math.floor(delta / 86_400_000)} d ago`
}

/** Keep a path readable in a narrow column: `<head>/.../<tail>`. */
export function shortenPath(path: string, max = 46): string {
  if (path.length <= max) return path
  const separator = path.includes('\\') ? '\\' : '/'
  const parts = path.split(separator)
  const tail = parts.at(-1) ?? path
  if (tail.length + 5 >= max) return `${path.slice(0, 3)}...${tail.slice(-(max - 6))}`
  const head = parts.slice(0, 2).join(separator)
  return `${head}${separator}...${separator}${tail}`
}
