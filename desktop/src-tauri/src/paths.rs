//! Where the runtime binary and its workspace come from.
//!
//! This module is the only place executable locations are decided. The
//! frontend cannot name a binary or pass an argument: it can only ask the host
//! to start, stop, or query the runtime it already resolved.

use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager};

use crate::state::HostError;

/// Name the bundled sidecar is installed under.
pub const BRIDGE_BINARY: &str = "rust-agent-bridge";

/// Name the sibling cargo binary builds to.
pub const BRIDGE_DEV_BINARY: &str = "desktop_bridge";

/// How many characters of a workspace's name survive into its store key.
const KEY_PREFIX_CHARS: usize = 40;

fn exe_suffix() -> &'static str {
    if cfg!(windows) {
        ".exe"
    } else {
        ""
    }
}

fn binary_name(stem: &str) -> String {
    format!("{stem}{}", exe_suffix())
}

/// Every place the host will look for the runtime, most specific first.
///
/// The order is: an explicit escape hatch for development, the sidecar the
/// bundler installs next to the app, then the sibling cargo target directory so
/// `tauri dev` works without a packaging step.
pub fn bridge_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    // Debug builds only: a packaged app must not be redirectable by the
    // environment it happens to inherit.
    #[cfg(debug_assertions)]
    if let Some(value) = std::env::var_os("RUST_AGENT_BRIDGE") {
        candidates.push(PathBuf::from(value));
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(binary_name(BRIDGE_BINARY)));
            // macOS app bundles keep helpers in Contents/Resources beside the
            // Contents/MacOS directory the executable lives in.
            candidates.push(
                dir.join("..")
                    .join("Resources")
                    .join(binary_name(BRIDGE_BINARY)),
            );
        }
    }

    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    for profile in ["debug", "release"] {
        candidates.push(
            repo.join("target")
                .join(profile)
                .join(binary_name(BRIDGE_DEV_BINARY)),
        );
    }

    candidates
}

/// The first candidate that exists, or an error naming all of them.
pub fn resolve_bridge() -> Result<PathBuf, HostError> {
    let candidates = bridge_candidates();
    if let Some(found) = candidates.iter().find(|path| path.is_file()) {
        return Ok(found.clone());
    }
    let looked: Vec<String> = candidates
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    Err(HostError::new(
        "runtime_not_found",
        format!(
            "could not find the rust-agent runtime. Build it with \
             `pnpm bridge:build` from desktop/. Looked in: {}",
            looked.join(", ")
        ),
    ))
}

/// The workspace override, when the environment sets one.
///
/// Debug builds only, and it must be absolute: this is a development
/// convenience, not a configuration channel for the UI.
pub fn workspace_override() -> Option<Result<PathBuf, HostError>> {
    #[cfg(debug_assertions)]
    {
        let value = std::env::var_os("RUST_AGENT_WORKSPACE")?;
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Some(Err(HostError::new(
                "workspace_override_invalid",
                "RUST_AGENT_WORKSPACE must be an absolute path",
            )));
        }
        if !path.is_dir() {
            return Some(Err(HostError::new(
                "workspace_override_invalid",
                format!(
                    "RUST_AGENT_WORKSPACE is not a directory: {}",
                    path.display()
                ),
            )));
        }
        Some(Ok(path))
    }
    #[cfg(not(debug_assertions))]
    {
        None
    }
}

/// The workspace the runtime is started with.
///
/// It lives under the per-user app data directory, never inside the install
/// directory, so memories, tasks, and logs survive upgrades and never need
/// write access to `Program Files` or `/Applications`.
pub fn resolve_workspace(app: &AppHandle) -> Result<PathBuf, HostError> {
    if let Some(override_) = workspace_override() {
        return override_;
    }

    let base = app.path().app_data_dir().map_err(|error| {
        HostError::new(
            "no_app_data_dir",
            format!("could not resolve the application data directory: {error}"),
        )
    })?;
    let workspace = base.join("workspace");
    std::fs::create_dir_all(&workspace).map_err(|error| {
        HostError::new(
            "workspace_unavailable",
            format!("could not create {}: {error}", workspace.display()),
        )
    })?;
    Ok(workspace)
}

/// The directory that holds one workspace's conversations.
///
/// Conversations are the app's, not the project's: they live under the per-user
/// app data directory, one directory per workspace, so a folder the runtime is
/// pointed at is never also a folder the runtime writes into. The key is derived
/// from the workspace path, so the same project always opens the same
/// conversations, and two projects with the same folder name do not share them.
pub fn sessions_dir(app: &AppHandle, workspace: &Path) -> Result<PathBuf, HostError> {
    let base = app.path().app_data_dir().map_err(|error| {
        HostError::new(
            "no_app_data_dir",
            format!("could not resolve the application data directory: {error}"),
        )
    })?;
    let dir = base.join("sessions").join(workspace_key(workspace));
    std::fs::create_dir_all(&dir).map_err(|error| {
        HostError::new(
            "sessions_dir_unavailable",
            format!("could not create {}: {error}", dir.display()),
        )
    })?;
    Ok(dir)
}

/// A stable, filesystem-safe name for one workspace.
///
/// The folder's own name comes first because it is what a human reads when
/// looking through the store, and the hash of the whole path is what actually
/// keeps two workspaces apart. Lower-cased on Windows, where the path the user
/// picked and the path the filesystem reports are the same folder.
fn workspace_key(workspace: &Path) -> String {
    let path = workspace.to_string_lossy();
    let path = if cfg!(windows) {
        path.to_lowercase()
    } else {
        path.into_owned()
    };
    let name = workspace
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let slug: String = name
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .take(KEY_PREFIX_CHARS)
        .collect();
    let slug = if slug.trim_matches('-').is_empty() {
        "workspace".to_string()
    } else {
        slug
    };
    format!("{slug}-{:016x}", fnv1a(path.as_bytes()))
}

/// FNV-1a over the path's bytes.
///
/// A directory name needs a hash that is stable across runs and cheap; it does
/// not need to be a cryptographic one, and this crate does not carry a hashing
/// dependency for a name.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_sidecar_is_looked_for_before_the_dev_build() {
        let candidates = bridge_candidates();
        let sidecar = candidates
            .iter()
            .position(|path| path.ends_with(binary_name(BRIDGE_BINARY)));
        let dev = candidates
            .iter()
            .position(|path| path.ends_with(format!("debug/{}", binary_name(BRIDGE_DEV_BINARY))));
        assert!(sidecar.is_some(), "no sidecar candidate: {candidates:?}");
        assert!(dev.is_some(), "no dev candidate: {candidates:?}");
        assert!(sidecar < dev);
    }

    #[test]
    fn the_binary_name_follows_the_platform() {
        let name = binary_name(BRIDGE_BINARY);
        if cfg!(windows) {
            assert!(name.ends_with(".exe"));
        } else {
            assert!(!name.contains('.'));
        }
    }

    #[test]
    fn a_missing_runtime_explains_every_place_it_looked() {
        // The dev candidates are the only ones that can exist under `cargo
        // test`, and this asserts the error shape rather than the result.
        if let Err(error) = resolve_bridge() {
            assert_eq!(error.code, "runtime_not_found");
            assert!(error.message.contains("pnpm bridge:build"));
        }
    }

    #[test]
    fn one_workspace_gets_one_store_key_whatever_the_path_looks_like() {
        let demo = Path::new("projects").join("demo");
        let key = workspace_key(&demo);
        assert_eq!(key, workspace_key(&demo));
        assert!(key.starts_with("demo-"), "{key}");

        // Two projects whose folders share a name are still two stores: the
        // hash is over the whole path, not just the last segment.
        assert_ne!(key, workspace_key(&Path::new("elsewhere").join("demo")));
        // A path with no name of its own still gets a usable key.
        assert!(workspace_key(Path::new("/")).starts_with("workspace-"));
    }

    #[test]
    fn a_store_key_is_a_name_a_filesystem_will_take() {
        let odd = Path::new("odd dir").join("na:me*?").join("folder. ");
        let key = workspace_key(&odd);
        assert!(!key.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']), "{key}");
        assert!(!key.ends_with('.') && !key.ends_with(' '), "{key}");
        assert!(key.len() <= KEY_PREFIX_CHARS + 17, "{key}");
        // One component, so joining it to a base directory cannot climb out of
        // it or land somewhere else than intended.
        assert_eq!(Path::new(&key).components().count(), 1, "{key}");
    }
}
