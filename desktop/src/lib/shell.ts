/**
 * Whether this page is running inside the desktop shell.
 *
 * Tauri injects `__TAURI_INTERNALS__` into the webview; a plain browser tab
 * pointed at the dev server does not have it, and every host call from there
 * fails with an opaque error. Both service modules guard on this so the UI can
 * say what is actually wrong instead of showing an empty list.
 */
export function isRunningInShell(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
}

/** Throw the one sentence that explains a missing shell, or return. */
export function requireShell(): void {
  if (!isRunningInShell()) {
    throw new Error(
      'The desktop shell is not running this page. Start it with `pnpm app:dev` ' +
        'instead of opening the Vite URL in a browser.',
    )
  }
}
