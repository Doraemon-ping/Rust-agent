//! The IPC surface.
//!
//! Every command is a thin wrapper: it resolves nothing on its own, takes no
//! paths or commands from the caller, and returns a coded error. If the
//! frontend can reach a capability, it is here, and it is this short.

use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;

use crate::paths;
use crate::projects::{ProjectList, ProjectsStore};
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

/// The folders the user added, plus which one is active.
#[tauri::command]
pub async fn projects_list(projects: State<'_, ProjectsStore>) -> Result<ProjectList, HostError> {
    Ok(projects.snapshot().await)
}

/// Open the system folder picker and add the folder the user chose.
///
/// `None` means the dialog was dismissed. The path comes back from the picker,
/// never from the caller: the page can ask for a dialog, but it cannot name a
/// directory.
#[tauri::command]
pub async fn projects_pick(
    app: AppHandle,
    projects: State<'_, ProjectsStore>,
    manager: State<'_, RuntimeManager>,
) -> Result<Option<ProjectList>, HostError> {
    let mut dialog = app.dialog().file().set_title("选择项目文件夹");
    // Parented to the window when there is one, so the picker comes up modal to
    // the app instead of somewhere behind it.
    if let Some(window) = app.get_webview_window("main") {
        dialog = dialog.set_parent(&window);
    }

    let Some(picked) = dialog.blocking_pick_folder() else {
        return Ok(None);
    };
    let path = picked.into_path().map_err(|error| {
        HostError::new(
            "project_path_unusable",
            format!("the picker returned a path the host cannot use: {error}"),
        )
    })?;

    let list = projects.add(&path).await?;
    resync_workspace(&manager).await;
    Ok(Some(list))
}

/// Make a stored project the active one.
#[tauri::command]
pub async fn projects_select(
    id: String,
    projects: State<'_, ProjectsStore>,
    manager: State<'_, RuntimeManager>,
) -> Result<ProjectList, HostError> {
    let list = projects.select(&id).await?;
    resync_workspace(&manager).await;
    Ok(list)
}

/// Forget a project. The folder itself is untouched.
#[tauri::command]
pub async fn projects_remove(
    id: String,
    projects: State<'_, ProjectsStore>,
    manager: State<'_, RuntimeManager>,
) -> Result<ProjectList, HostError> {
    let list = projects.remove(&id).await?;
    resync_workspace(&manager).await;
    Ok(list)
}

/// Point a live runtime at the newly active project.
///
/// A restart that fails is already recorded in the runtime status and pushed to
/// the UI from there, and it does not undo the project change, so it is not an
/// error here: the list the caller asked for is still valid.
async fn resync_workspace(manager: &RuntimeManager) {
    if let Err(error) = manager.apply_workspace_change().await {
        eprintln!("rust-agent-desktop: {}", error.message);
    }
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
