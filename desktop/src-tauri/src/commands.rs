//! The IPC surface.
//!
//! Every command is a thin wrapper: it resolves nothing on its own, takes no
//! paths or commands from the caller, and returns a coded error. If the
//! frontend can reach a capability, it is here, and it is this short.

use serde_json::Value;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;

use crate::paths;
use crate::projects::{ProjectList, ProjectsStore};
use crate::provider::{ProviderInput, ProviderSettings, ProviderStore};
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

/// Send one prompt to the runtime.
///
/// The prompt is the only thing the page may choose here: the workspace, the
/// model, and the credentials are all decided on this side. The answer streams
/// back as `chat.*` events, so this returns as soon as the turn is accepted.
#[tauri::command]
pub async fn chat_send(
    prompt: String,
    manager: State<'_, RuntimeManager>,
) -> Result<Value, HostError> {
    manager.chat_send(&prompt).await
}

/// The conversation the runtime is holding.
///
/// A page that reloaded asks for this instead of losing what it was only ever a
/// view of. The page still names nothing: the turns come from the runtime this
/// host started, in the workspace this host chose.
#[tauri::command]
pub async fn chat_history(manager: State<'_, RuntimeManager>) -> Result<Value, HostError> {
    manager.chat_history().await
}

/// Stop the turn that is running.
///
/// Nothing about the turn is chosen here: the runtime stops the one turn it is
/// running, and says whether there was one.
#[tauri::command]
pub async fn chat_cancel(manager: State<'_, RuntimeManager>) -> Result<Value, HostError> {
    manager.chat_cancel().await
}

/// The conversations this workspace holds, and the one being continued.
#[tauri::command]
pub async fn chat_sessions(manager: State<'_, RuntimeManager>) -> Result<Value, HostError> {
    manager.chat_sessions().await
}

/// Start a fresh conversation in the runtime's workspace.
#[tauri::command]
pub async fn chat_new_session(manager: State<'_, RuntimeManager>) -> Result<Value, HostError> {
    manager.chat_new_session().await
}

/// Continue a conversation this workspace already holds.
///
/// The page may name a session here, and that is deliberate: a session is one
/// the runtime itself listed, and the id is checked against the workspace on
/// the other side before anything is read.
#[tauri::command]
pub async fn chat_open_session(
    id: String,
    manager: State<'_, RuntimeManager>,
) -> Result<Value, HostError> {
    manager.chat_open_session(&id).await
}

/// Rename a conversation this workspace already holds.
///
/// The page names a conversation the runtime itself listed, and the words to
/// call it: a name is not a path, and it is bounded on the other side.
#[tauri::command]
pub async fn chat_rename_session(
    id: String,
    title: String,
    manager: State<'_, RuntimeManager>,
) -> Result<Value, HostError> {
    manager.chat_rename_session(&id, &title).await
}

/// Forget a conversation. The answer says which one is being continued now.
///
/// Deleting is not an undo, so the page asks once and means it: what goes is one
/// conversation the runtime listed, and the folder the user chose is untouched.
#[tauri::command]
pub async fn chat_delete_session(
    id: String,
    manager: State<'_, RuntimeManager>,
) -> Result<Value, HostError> {
    manager.chat_delete_session(&id).await
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
    resync_runtime(&manager).await;
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
    resync_runtime(&manager).await;
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
    resync_runtime(&manager).await;
    Ok(list)
}

/// Point a live runtime at the change that was just made.
///
/// The workspace and the environment are both decided when the child is
/// spawned, so a project or a credential only takes effect in a new process. A
/// restart that fails is already recorded in the runtime status and pushed to
/// the UI from there, and it does not undo the change, so it is not an error
/// here: what the caller asked for is still stored and still valid.
async fn resync_runtime(manager: &RuntimeManager) {
    if let Err(error) = manager.apply_change().await {
        eprintln!("rust-agent-desktop: {}", error.message);
    }
}

/// The model settings the runtime will be started with.
#[tauri::command]
pub async fn provider_get(provider: State<'_, ProviderStore>) -> Result<ProviderSettings, HostError> {
    Ok(provider.snapshot().await)
}

/// Store the settings the user saved. The API key is write-only: whatever is
/// sent here is never sent back to the page.
#[tauri::command]
pub async fn provider_save(
    settings: ProviderInput,
    provider: State<'_, ProviderStore>,
    manager: State<'_, RuntimeManager>,
) -> Result<ProviderSettings, HostError> {
    let saved = provider.save(settings).await?;
    resync_runtime(&manager).await;
    Ok(saved)
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
