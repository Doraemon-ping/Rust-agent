/**
 * The model settings, as the page sees them.
 *
 * One of the seam modules: alongside `lib/runtime` and `lib/projects`, these
 * are the only files that import `@tauri-apps/api`. The page reads the settings
 * and saves them; it never sees the stored key, and it does not decide the
 * environment variable names -- it shows the ones the host reports.
 */

import { invoke } from '@tauri-apps/api/core'

import { requireShell } from '@/lib/shell'
import type { ProviderInput, ProviderSettings } from './types'

/** The settings the runtime will be started with. */
export async function getProviderSettings(): Promise<ProviderSettings> {
  requireShell()
  return invoke<ProviderSettings>('provider_get')
}

/** Store the settings the user saved. A live runtime is restarted into them. */
export async function saveProviderSettings(input: ProviderInput): Promise<ProviderSettings> {
  requireShell()
  return invoke<ProviderSettings>('provider_save', { settings: input })
}

export type { ProviderInput, ProviderSettings } from './types'
