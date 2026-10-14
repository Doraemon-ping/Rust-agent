/**
 * The Settings page's model-provider shapes, mirroring
 * `src-tauri/src/provider.rs`.
 *
 * The key is the one fact that only travels one way: the page can store one and
 * can ask whether one is stored, but the host never sends the value back. That
 * is why `ProviderInput.apiKey` has three states instead of two.
 */

/** The settings as the host reports them. Note what is missing: the key. */
export interface ProviderSettings {
  /** Base URL of an Anthropic-compatible Messages API endpoint. */
  baseUrl: string
  model: string | null
  fallbackModel: string | null
  maxTokens: number | null
  hasKey: boolean
  /** The last four characters of a stored key, when it is long enough. */
  keyHint: string | null
  /** RFC3339, stamped on every save. */
  updatedAt: string | null
  /** Where the host keeps the store, so the user can find it. */
  path: string
  /** The environment variables the host sets on the runtime, in order. */
  env: string[]
  /** Non-fatal problems with the store, worth showing as they are. */
  warnings: string[]
}

/** What the page sends up on save. An empty text field means "not set". */
export interface ProviderInput {
  baseUrl: string
  model: string
  fallbackModel: string
  maxTokens: number | null
  /**
   * `null` keeps the stored key, `''` clears it, anything else replaces it.
   * Empty cannot mean "keep": the page never has the stored key to send back.
   */
  apiKey: string | null
}
