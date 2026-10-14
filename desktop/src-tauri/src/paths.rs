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
}
