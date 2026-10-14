//! The IPC surface.
//!
//! Every command is a thin wrapper: it resolves nothing on its own, takes no
//! paths or commands from the caller, and returns a coded error. If the
//! frontend can reach a capability, it is here, and it is this short.

use tauri::{AppHandle, State};

use crate::paths;
use crate::runtime::RuntimeManager;
use crate::state::{HostError, HostInfo, LogLine, Ping, Status, PROTOCOL_VERSION};

/// Current host-side status. Works even when the runtime is stopped.
#[tauri::command]
pub async fn runtime_status(manager: State<'_, RuntimeManager>) -> Result<Status, HostError> {
    Ok(manager.status().await)
}

/// Start the runtime. Idempotent while it is already running.
#[tauri::command]
pub async fn runtime_start(manager: State<'_, RuntimeManager>) -> Result<Status, HostError> {
    manager.start().await
}

/// Stop the runtime and wait for the process to exit.
#[tauri::command]
pub async fn runtime_stop(manager: State<'_, RuntimeManager>) -> Result<Status, HostError> {
    manager.stop().await
}

/// Stop, then start.
#[tauri::command]
pub async fn runtime_restart(manager: State<'_, RuntimeManager>) -> Result<Status, HostError> {
    manager.restart().await
}

/// Round-trip a ping through the runtime.
#[tauri::command]
pub async fn runtime_ping(manager: State<'_, RuntimeManager>) -> Result<Ping, HostError> {
    manager.ping().await
}

/// The most recent log lines, oldest first.
#[tauri::command]
pub async fn runtime_logs(
    manager: State<'_, RuntimeManager>,
    limit: Option<usize>,
) -> Result<Vec<LogLine>, HostError> {
    Ok(manager.logs(limit.unwrap_or(200).min(1000)).await)
}

/// Facts about the shell: versions, platform, workspace, and where the runtime
/// binary is expected. Read-only, so the UI can explain a start failure.
#[tauri::command]
pub async fn runtime_host_info(
    app: AppHandle,
    manager: State<'_, RuntimeManager>,
) -> Result<HostInfo, HostError> {
    // Touching the manager keeps this command honest about who owns the
    // lifecycle, and warms the runtime status for the caller.
    let _ = manager.status().await;

    let workspace = paths::resolve_workspace(&app)?;
    Ok(HostInfo {
        app_version: app.package_info().version.to_string(),
        tauri_version: tauri::VERSION.to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        protocol_version: PROTOCOL_VERSION,
        workspace: workspace.display().to_string(),
        bridge_candidates: paths::bridge_candidates()
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
    })
}
