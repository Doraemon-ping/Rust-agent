//! The model provider settings the user edits on the Settings page.
//!
//! The runtime reads its endpoint, credentials, and model from the process
//! environment (`ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY`, `MODEL_ID`,
//! `FALLBACK_MODEL_ID`, `MAX_TOKENS`). The shell owns that environment for the
//! child process it starts, so this store is where those values come from.
//!
//! The key travels one way. The page can store one and can ask whether one is
//! stored, but the value is never sent back to the frontend: the view carries
//! `has_key` and a short hint instead. An empty field means "not set" for every
//! field, which lets the runtime apply its own default rather than freezing
//! today's default into the user's settings.
//!
//! The store is small, human-readable JSON under the per-user app data
//! directory. A file this build cannot read is never overwritten: mutations
//! refuse until someone deals with it, so a parse bug cannot eat the settings.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tokio::sync::Mutex;

use crate::state::HostError;

/// Shape of the on-disk file. Bump when older builds can no longer read it.
const FILE_VERSION: u32 = 1;
const FILE_NAME: &str = "provider.json";

/// The environment variables the shell sets on the runtime, in that order.
pub const ENV_BASE_URL: &str = "ANTHROPIC_BASE_URL";
pub const ENV_API_KEY: &str = "ANTHROPIC_API_KEY";
pub const ENV_MODEL: &str = "MODEL_ID";
pub const ENV_FALLBACK_MODEL: &str = "FALLBACK_MODEL_ID";
pub const ENV_MAX_TOKENS: &str = "MAX_TOKENS";

const ENV_ORDER: [&str; 5] = [
    ENV_BASE_URL,
    ENV_API_KEY,
    ENV_MODEL,
    ENV_FALLBACK_MODEL,
    ENV_MAX_TOKENS,
];

/// Most response tokens the page may ask for. A slipped keystroke should not
/// quietly become the model's whole context window.
const MAX_TOKENS_LIMIT: u32 = 1_000_000;

/// The settings as the frontend sees them. Note what is missing: the key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSettings {
    pub base_url: String,
    pub model: Option<String>,
    pub fallback_model: Option<String>,
    pub max_tokens: Option<u32>,
    /// Whether a key is stored. The key itself is never sent to the page.
    pub has_key: bool,
    /// The last four characters of a stored key, when it is long enough that
    /// they cannot be the whole thing. Enough to tell two keys apart.
    pub key_hint: Option<String>,
    pub updated_at: Option<String>,
    /// Where the store lives, so the user can find it.
    pub path: String,
    /// The runtime variables the shell sets, in order. Read-only: the page
    /// shows the mapping, it does not choose it.
    pub env: Vec<String>,
    /// Non-fatal problems worth showing: an unreadable store, mostly.
    pub warnings: Vec<String>,
}

/// What the page sends up when the user saves.
///
/// Every text field is trimmed, and an empty one means "not set". The key has
/// three states on purpose: `None` keeps whatever is stored, `Some("")` clears
/// it, and anything else replaces it. Empty cannot mean "keep" here, because
/// the page never sees the stored key to send it back.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInput {
    pub base_url: String,
    pub model: String,
    pub fallback_model: String,
    pub max_tokens: Option<u32>,
    pub api_key: Option<String>,
}

/// The on-disk shape. `updatedAt` is stamped on every write.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct File {
    version: u32,
    base_url: String,
    api_key: Option<String>,
    model: Option<String>,
    fallback_model: Option<String>,
    max_tokens: Option<u32>,
    updated_at: Option<String>,
}

impl File {
    /// A store with nothing set but the endpoint the runtime itself defaults
    /// to, so the page opens on a value rather than on a blank field.
    fn empty() -> Self {
        File {
            version: FILE_VERSION,
            base_url: "https://api.anthropic.com".to_string(),
            api_key: None,
            model: None,
            fallback_model: None,
            max_tokens: None,
            updated_at: None,
        }
    }
}

struct Inner {
    file: File,
    /// Set when the file exists but cannot be used. Mutations refuse while it
    /// is set, so a corrupt file is never silently replaced.
    unreadable: Option<String>,
}

impl Inner {
    fn view(&self, path: &Path) -> ProviderSettings {
        ProviderSettings {
            base_url: self.file.base_url.clone(),
            model: self.file.model.clone(),
            fallback_model: self.file.fallback_model.clone(),
            max_tokens: self.file.max_tokens,
            has_key: self.file.api_key.is_some(),
            key_hint: self.file.api_key.as_deref().and_then(key_hint),
            updated_at: self.file.updated_at.clone(),
            path: path.display().to_string(),
            env: ENV_ORDER.map(str::to_string).to_vec(),
            warnings: self.unreadable.iter().cloned().collect(),
        }
    }

    /// The variables the shell will set on the runtime. A field that was never
    /// set is left out rather than sent empty, so the runtime's own default
    /// applies instead of a blank value.
    fn env(&self) -> Vec<(&'static str, String)> {
        let mut pairs: Vec<(&'static str, String)> = Vec::new();
        let base_url = self.file.base_url.trim();
        if !base_url.is_empty() {
            pairs.push((ENV_BASE_URL, base_url.to_string()));
        }
        if let Some(key) = self.file.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
            pairs.push((ENV_API_KEY, key.to_string()));
        }
        if let Some(model) = self.file.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
            pairs.push((ENV_MODEL, model.to_string()));
        }
        if let Some(model) = self
            .file
            .fallback_model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            pairs.push((ENV_FALLBACK_MODEL, model.to_string()));
        }
        if let Some(tokens) = self.file.max_tokens {
            pairs.push((ENV_MAX_TOKENS, tokens.to_string()));
        }
        pairs
    }

    /// Refuse to touch a store that could not be read.
    fn guard_writable(&self) -> Result<(), HostError> {
        match &self.unreadable {
            Some(reason) => Err(HostError::new(
                "provider_store_unreadable",
                format!("{reason}; the host will not rewrite it until that is dealt with"),
            )),
            None => Ok(()),
        }
    }
}
/// The host's handle on the provider settings. Cheap to clone.
#[derive(Clone)]
pub struct ProviderStore {
    path: PathBuf,
    inner: Arc<Mutex<Inner>>,
}

impl ProviderStore {
    /// Open the store under the per-user app data directory.
    pub fn new(app: &AppHandle) -> Result<Self, HostError> {
        let dir = app.path().app_data_dir().map_err(|error| {
            HostError::new(
                "no_app_data_dir",
                format!("could not resolve the application data directory: {error}"),
            )
        })?;
        Self::open(&dir)
    }

    /// Open (or create) a store in `dir`. Split from [`ProviderStore::new`] so
    /// the store can be tested without a running app.
    pub fn open(dir: &Path) -> Result<Self, HostError> {
        std::fs::create_dir_all(dir).map_err(|error| {
            HostError::new(
                "provider_unavailable",
                format!("could not create {}: {error}", dir.display()),
            )
        })?;
        let path = dir.join(FILE_NAME);
        let (file, unreadable) = read(&path);
        Ok(ProviderStore {
            path,
            inner: Arc::new(Mutex::new(Inner { file, unreadable })),
        })
    }

    /// The settings as the UI should show them: values, whether a key is
    /// stored, and the hint that goes with it.
    pub async fn snapshot(&self) -> ProviderSettings {
        self.inner.lock().await.view(&self.path)
    }

    /// The variables the shell sets on the runtime child process.
    pub async fn env(&self) -> Vec<(&'static str, String)> {
        self.inner.lock().await.env()
    }

    /// Validate, persist, then adopt. A failed write leaves the in-memory
    /// settings as they were, so the UI never shows a save disk did not take.
    pub async fn save(&self, input: ProviderInput) -> Result<ProviderSettings, HostError> {
        let base_url = input.base_url.trim().to_string();
        if !base_url.is_empty() && !is_http_url(&base_url) {
            return Err(HostError::new(
                "provider_base_url_invalid",
                format!("the base URL must start with http:// or https://: {base_url}"),
            ));
        }
        if let Some(tokens) = input.max_tokens {
            if tokens == 0 || tokens > MAX_TOKENS_LIMIT {
                return Err(HostError::new(
                    "provider_max_tokens_invalid",
                    format!("the token budget must be between 1 and {MAX_TOKENS_LIMIT}: {tokens}"),
                ));
            }
        }

        let mut inner = self.inner.lock().await;
        inner.guard_writable()?;
        let mut file = inner.file.clone();
        file.base_url = base_url;
        file.model = optional(input.model);
        file.fallback_model = optional(input.fallback_model);
        file.max_tokens = input.max_tokens;
        match input.api_key {
            // Not sent: keep what is stored.
            None => {}
            // Sent empty: the user cleared it.
            Some(key) => file.api_key = optional(key),
        }
        file.updated_at = Some(crate::state::now());
        self.commit(&mut inner, file).await
    }

    /// Persist, then adopt.
    async fn commit(&self, inner: &mut Inner, file: File) -> Result<ProviderSettings, HostError> {
        write(&self.path, &file)?;
        inner.file = file;
        Ok(inner.view(&self.path))
    }
}

/// Trimmed, or nothing when the field was left empty.
fn optional(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn is_http_url(value: &str) -> bool {
    let rest = match value.strip_prefix("http://") {
        Some(rest) => rest,
        None => match value.strip_prefix("https://") {
            Some(rest) => rest,
            None => return false,
        },
    };
    !rest.is_empty() && !rest.starts_with('/')
}

/// The last four characters of a key, when it is long enough that they cannot
/// be the whole thing.
fn key_hint(key: &str) -> Option<String> {
    let count = key.chars().count();
    if count < 8 {
        return None;
    }
    Some(key.chars().skip(count - 4).collect())
}
/// Read the store. A missing file is the normal first run; anything else leaves
/// a reason and the defaults.
fn read(path: &Path) -> (File, Option<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (File::empty(), None),
        Err(error) => {
            return (
                File::empty(),
                Some(format!("could not read {}: {error}", path.display())),
            )
        }
    };
    match serde_json::from_str::<File>(&text) {
        Ok(file) if file.version == FILE_VERSION => (file, None),
        Ok(file) => (
            File::empty(),
            Some(format!(
                "{} is version {}, this build speaks version {FILE_VERSION}",
                path.display(),
                file.version
            )),
        ),
        Err(error) => (
            File::empty(),
            Some(format!("{} is not valid JSON: {error}", path.display())),
        ),
    }
}

fn write(path: &Path, file: &File) -> Result<(), HostError> {
    let text = serde_json::to_string_pretty(file).map_err(|error| {
        HostError::new(
            "provider_encode_failed",
            format!("could not encode the provider settings: {error}"),
        )
    })?;
    std::fs::write(path, format!("{text}\n")).map_err(|error| {
        HostError::new(
            "provider_write_failed",
            format!("could not write {}: {error}", path.display()),
        )
    })?;
    restrict(path)
}

/// A key sits in this file, so on Unix it is readable only by its owner.
/// Windows inherits the per-user ACL of the app data directory already.
#[cfg(unix)]
fn restrict(path: &Path) -> Result<(), HostError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
        HostError::new(
            "provider_write_failed",
            format!("could not restrict {}: {error}", path.display()),
        )
    })
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<(), HostError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh store in its own directory.
    fn scratch(tag: &str) -> (ProviderStore, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "rust-agent-provider-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        (ProviderStore::open(&root).unwrap(), root)
    }

    /// A fillable save: nothing set that a test does not ask for.
    fn input() -> ProviderInput {
        ProviderInput {
            base_url: "https://api.example.com".to_string(),
            model: String::new(),
            fallback_model: String::new(),
            max_tokens: None,
            api_key: None,
        }
    }

    fn value<'a>(pairs: &'a [(&'static str, String)], key: &str) -> Option<&'a str> {
        pairs
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.as_str())
    }

    #[tokio::test]
    async fn a_fresh_store_offers_the_runtime_default_endpoint() {
        let (store, _dir) = scratch("fresh");

        let view = store.snapshot().await;
        assert_eq!(view.base_url, "https://api.anthropic.com");
        assert_eq!(view.model, None);
        assert!(!view.has_key);
        assert_eq!(view.key_hint, None);
        assert_eq!(view.updated_at, None);
        assert!(view.warnings.is_empty(), "{view:?}");
        assert_eq!(view.env, ENV_ORDER.map(str::to_string).to_vec());
        assert!(view.path.ends_with(FILE_NAME));
    }

    #[tokio::test]
    async fn a_stored_key_is_reported_but_never_sent_back() {
        let (store, _dir) = scratch("key");
        let mut input = input();
        input.api_key = Some("sk-ant-verysecret-9876".to_string());

        let view = store.save(input).await.unwrap();
        assert!(view.has_key);
        assert_eq!(view.key_hint.as_deref(), Some("9876"));

        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("verysecret"), "{json}");
        assert!(!json.contains("apiKey"), "{json}");
    }

    #[tokio::test]
    async fn an_absent_key_is_kept_and_an_empty_one_clears_it() {
        let (store, _dir) = scratch("keep");
        let mut first = input();
        first.api_key = Some("sk-ant-keep-me-1234".to_string());
        store.save(first).await.unwrap();

        // The page saves again without touching the key field.
        let mut second = input();
        second.model = "claude-sonnet-4-5".to_string();
        let view = store.save(second).await.unwrap();
        assert!(view.has_key);
        assert_eq!(view.model.as_deref(), Some("claude-sonnet-4-5"));

        // The clear button: the page sends the key as empty on purpose.
        let mut third = input();
        third.api_key = Some(String::new());
        let view = store.save(third).await.unwrap();
        assert!(!view.has_key);
        assert_eq!(view.key_hint, None);
    }

    #[tokio::test]
    async fn only_set_fields_reach_the_runtime_environment() {
        let (store, _dir) = scratch("env");
        let mut input = input();
        input.api_key = Some("sk-ant-env-1234".to_string());
        input.model = "claude-sonnet-4-5".to_string();
        input.max_tokens = Some(4096);
        store.save(input).await.unwrap();

        let pairs = store.env().await;
        assert_eq!(value(&pairs, ENV_BASE_URL), Some("https://api.example.com"));
        assert_eq!(value(&pairs, ENV_API_KEY), Some("sk-ant-env-1234"));
        assert_eq!(value(&pairs, ENV_MODEL), Some("claude-sonnet-4-5"));
        assert_eq!(value(&pairs, ENV_MAX_TOKENS), Some("4096"));
        // Never set, so the runtime keeps its own default rather than "".
        assert_eq!(value(&pairs, ENV_FALLBACK_MODEL), None);
    }

    #[tokio::test]
    async fn blank_fields_are_stored_as_unset() {
        let (store, _dir) = scratch("blank");
        let mut input = input();
        input.base_url = "  ".to_string();
        input.model = "  ".to_string();
        input.fallback_model = "  haiku  ".to_string();

        let view = store.save(input).await.unwrap();
        assert_eq!(view.base_url, "");
        assert_eq!(view.model, None);
        assert_eq!(view.fallback_model.as_deref(), Some("haiku"));
        // Nothing but the fallback model was set, so nothing else is handed on.
        assert_eq!(store.env().await.len(), 1);
    }

    #[tokio::test]
    async fn a_refused_save_leaves_the_store_untouched() {
        let (store, dir) = scratch("refuse");
        let mut good = input();
        good.model = "claude-sonnet-4-5".to_string();
        store.save(good).await.unwrap();

        let mut bad = input();
        bad.base_url = "api.example.com".to_string();
        let error = store.save(bad.clone()).await.unwrap_err();
        assert_eq!(error.code, "provider_base_url_invalid");

        bad.base_url = "https://api.example.com".to_string();
        bad.base_url = "https://evil.example.com".to_string();
        bad.max_tokens = Some(0);
        let error = store.save(bad.clone()).await.unwrap_err();
        assert_eq!(error.code, "provider_max_tokens_invalid");

        bad.max_tokens = Some(MAX_TOKENS_LIMIT + 1);
        let error = store.save(bad.clone()).await.unwrap_err();
        assert_eq!(error.code, "provider_max_tokens_invalid");

        // A refused save touched neither memory nor disk.
        let view = store.snapshot().await;
        assert_eq!(view.model.as_deref(), Some("claude-sonnet-4-5"));
        let text = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        assert!(!text.contains("evil.example.com"), "{text}");
    }

    #[test]
    fn a_base_url_needs_a_host_and_a_plain_http_scheme() {
        assert!(is_http_url("https://api.anthropic.com"));
        assert!(is_http_url("http://localhost:8080"));
        assert!(!is_http_url("api.anthropic.com"));
        assert!(!is_http_url("https://"));
        assert!(!is_http_url("https:///v1"));
        assert!(!is_http_url("ftp://example.com"));
    }

    #[test]
    fn a_short_key_gets_no_hint() {
        assert_eq!(key_hint("sk-abc"), None);
        assert_eq!(key_hint("12345678"), Some("5678".to_string()));
        assert_eq!(key_hint("sk-ant-api03-abcdef"), Some("cdef".to_string()));
    }

    #[tokio::test]
    async fn the_settings_survive_a_reopen() {
        let (store, dir) = scratch("reopen");
        let mut input = input();
        input.model = "claude-sonnet-4-5".to_string();
        input.fallback_model = "claude-haiku-4-5".to_string();
        input.max_tokens = Some(2048);
        input.api_key = Some("sk-ant-reopen-9999".to_string());
        let saved = store.save(input).await.unwrap();

        let reopened = ProviderStore::open(&dir).unwrap().snapshot().await;
        assert_eq!(reopened.model, saved.model);
        assert_eq!(reopened.fallback_model, saved.fallback_model);
        assert_eq!(reopened.max_tokens, saved.max_tokens);
        assert_eq!(reopened.base_url, saved.base_url);
        assert!(reopened.has_key);
        assert_eq!(reopened.key_hint.as_deref(), Some("9999"));
        assert_eq!(reopened.updated_at, saved.updated_at);
    }

    #[tokio::test]
    async fn the_stored_file_is_plain_json_keyed_for_the_frontend() {
        let (store, dir) = scratch("shape");
        let mut input = input();
        input.model = "claude-sonnet-4-5".to_string();
        store.save(input).await.unwrap();

        let text = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["version"], serde_json::json!(1));
        assert_eq!(value["model"], serde_json::json!("claude-sonnet-4-5"));
        assert!(value["updatedAt"].is_string());
        // The key is on disk, in this per-user file, and nowhere else.
        assert!(value["apiKey"].is_null());
    }

    #[tokio::test]
    async fn a_corrupt_store_is_reported_and_left_alone() {
        let (store, dir) = scratch("corrupt");
        let mut input = input();
        input.model = "claude-sonnet-4-5".to_string();
        store.save(input.clone()).await.unwrap();
        let file = dir.join(FILE_NAME);
        std::fs::write(&file, "{ not json").unwrap();

        let reopened = ProviderStore::open(&dir).unwrap();
        let view = reopened.snapshot().await;
        assert_eq!(view.model, None);
        assert_eq!(view.warnings.len(), 1);

        let error = reopened.save(input).await.unwrap_err();
        assert_eq!(error.code, "provider_store_unreadable");
        // Left alone, not repaired: the bytes are still there for a person to
        // look at, and no key is silently replaced with an empty one.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{ not json");
    }

    #[tokio::test]
    async fn a_future_version_is_refused_rather_than_guessed() {
        let (_store, dir) = scratch("version");
        std::fs::write(
            dir.join(FILE_NAME),
            r#"{"version":99,"baseUrl":"https://x.example.com","apiKey":null,"model":null,"fallbackModel":null,"maxTokens":null,"updatedAt":null}"#,
        )
        .unwrap();

        let view = ProviderStore::open(&dir).unwrap().snapshot().await;
        assert_eq!(view.warnings.len(), 1);
        assert!(view.warnings[0].contains("version 99"), "{view:?}");
        assert_eq!(view.model, None);
    }
}
