//! The desktop shell.
//!
//! It hosts one thing: the rust-agent runtime as a child process, reachable
//! over JSON Lines. The shell owns that process's lifetime; the page can only
//! observe it and ask for changes through [`commands`].

mod commands;
mod paths;
mod projects;
mod provider;
mod protocol;
mod runtime;
mod state;

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{Manager, RunEvent};

use projects::ProjectsStore;
use provider::ProviderStore;
use runtime::RuntimeManager;

/// Set once the exit cleanup has been scheduled, so a second close request
/// cannot start it twice.
static FINISHING: AtomicBool = AtomicBool::new(false);

/// Build and run the shell. Called from `main`.
pub fn run() {
    tauri::Builder::default()
        // The host opens the folder picker on the page's behalf; the page
        // itself is granted no dialog, filesystem, or shell permission.
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // The project list outlives a run, so it is opened before anything
            // can read it -- including the runtime, which is started in the
            // active project.
            let projects = ProjectsStore::new(app.handle()).map_err(|error| {
                format!(
                    "could not open the project list: {}: {}",
                    error.code, error.message
                )
            })?;
            app.manage(projects.clone());

            // The runtime's environment comes from this store, so it is opened
            // with the same care and before anything can start a child.
            let provider = ProviderStore::new(app.handle()).map_err(|error| {
                format!(
                    "could not open the provider settings: {}: {}",
                    error.code, error.message
                )
            })?;
            app.manage(provider.clone());

            let manager = RuntimeManager::new(app.handle().clone(), projects, provider);
            app.manage(manager.clone());

            // The host starts the runtime, not the page: the UI never names a
            // binary, so there is no path from the frontend to a process.
            tauri::async_runtime::spawn(async move {
                if let Err(error) = manager.start().await {
                    // Already recorded in the status and shown in the UI; this
                    // is the host-side trace.
                    eprintln!("rust-agent-desktop: {}", error.message);
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::runtime_status,
            commands::runtime_start,
            commands::runtime_stop,
            commands::runtime_restart,
            commands::runtime_ping,
            commands::runtime_logs,
            commands::runtime_host_info,
            commands::chat_send,
            commands::chat_history,
            commands::chat_cancel,
            commands::chat_sessions,
            commands::chat_new_session,
            commands::chat_open_session,
            commands::projects_list,
            commands::projects_pick,
            commands::projects_select,
            commands::projects_remove,
            commands::provider_get,
            commands::provider_save,
        ])
        .build(tauri::generate_context!())
        .expect("could not build the desktop shell")
        .run(|app, event| {
            // Ordered cleanup, scheduled off the event-loop thread.
            //
            // Stopping the runtime emits the status changes that go with it,
            // and an emit waits for the event loop to run it. Doing this from
            // `RunEvent::Exit` -- on the loop thread -- would wait on the very
            // loop we are holding: the app would never finish exiting and the
            // runtime would be left behind. So refuse the exit once, stop the
            // runtime on the async runtime, then ask to exit again; that second
            // request carries `code: Some(..)` and goes straight through.
            let RunEvent::ExitRequested { api, code, .. } = event else {
                return;
            };
            if code.is_some() || FINISHING.swap(true, Ordering::SeqCst) {
                return;
            }
            api.prevent_exit();
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Some(manager) = app.try_state::<RuntimeManager>() {
                    manager.inner().clone().shutdown().await;
                }
                app.exit(0);
            });
        });
}
