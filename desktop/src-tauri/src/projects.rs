//! The folders the user added as projects.
//!
//! A project is the folder the runtime is started in, so this list is more than
//! bookmarks: choosing one changes the runtime's workspace. The page never
//! names a path -- it asks the host to open the system picker, and then chooses
//! among the paths the host already stores.
//!
//! The store is small, human-readable JSON under the per-user app data
//! directory. A file this build cannot read is never overwritten: mutations
//! refuse until someone deals with it, so a parse bug cannot eat the list.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tokio::sync::Mutex;

use crate::state::HostError;

/// Shape of the on-disk file. Bump when older builds can no longer read it.
const FILE_VERSION: u32 = 1;
const FILE_NAME: &str = "projects.json";

/// One folder, as the frontend sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    /// The host's own handle on the folder: its absolute path.
    pub id: String,
    pub name: String,
    pub path: String,
    pub added_at: String,
    /// Whether the folder is still there. A missing project is flagged, never
    /// dropped: the host does not get to delete what the user added.
    pub exists: bool,
}

/// The list, plus the two extra facts the UI needs to render it honestly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectList {
    pub items: Vec<Project>,
    pub active_id: Option<String>,
    /// Non-fatal problems worth showing: an unreadable store, mostly.
    pub warnings: Vec<String>,
}

/// The on-disk shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct File {
    version: u32,
    active_id: Option<String>,
    items: Vec<Stored>,
}

impl File {
    fn empty() -> Self {
        File {
            version: FILE_VERSION,
            active_id: None,
            items: Vec::new(),
        }
    }
}

/// One entry as it is stored. `exists` describes the moment, not the folder, so
/// it is not persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stored {
    id: String,
    name: String,
    path: String,
    added_at: String,
}

struct Inner {
    file: File,
    /// Set when the file exists but cannot be used. Mutations refuse while it
    /// is set, so a corrupt file is never silently replaced.
    unreadable: Option<String>,
}

impl Inner {
    fn snapshot(&self) -> ProjectList {
        ProjectList {
            items: self
                .file
                .items
                .iter()
                .map(|item| Project {
                    id: item.id.clone(),
                    name: item.name.clone(),
                    path: item.path.clone(),
                    added_at: item.added_at.clone(),
                    exists: Path::new(&item.path).is_dir(),
                })
                .collect(),
            active_id: self.file.active_id.clone(),
            warnings: self.unreadable.iter().cloned().collect(),
        }
    }

    /// Refuse to touch a store that could not be read.
    fn guard_writable(&self) -> Result<(), HostError> {
        match &self.unreadable {
            Some(reason) => Err(HostError::new(
                "projects_store_unreadable",
                format!("{reason}; the host will not rewrite it until that is dealt with"),
            )),
            None => Ok(()),
        }
    }
}

/// The host's handle on the project list. Cheap to clone.
#[derive(Clone)]
pub struct ProjectsStore {
    path: PathBuf,
    inner: Arc<Mutex<Inner>>,
}

impl ProjectsStore {
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

    /// Open (or create) a store in `dir`. Split from [`ProjectsStore::new`] so
    /// the store can be tested without a running app.
    pub fn open(dir: &Path) -> Result<Self, HostError> {
        std::fs::create_dir_all(dir).map_err(|error| {
            HostError::new(
                "projects_unavailable",
                format!("could not create {}: {error}", dir.display()),
            )
        })?;
        let path = dir.join(FILE_NAME);
        let (file, unreadable) = read(&path);
        Ok(ProjectsStore {
            path,
            inner: Arc::new(Mutex::new(Inner { file, unreadable })),
        })
    }

    /// The list as the UI should show it: stored order, with `exists` decided
    /// now rather than when the entry was added.
    pub async fn snapshot(&self) -> ProjectList {
        self.inner.lock().await.snapshot()
    }

    /// The folder the runtime should use, when the user has chosen one.
    pub async fn active_path(&self) -> Option<PathBuf> {
        let inner = self.inner.lock().await;
        let active = inner.file.active_id.as_deref()?;
        inner
            .file
            .items
            .iter()
            .find(|item| same_path(&item.id, active))
            .map(|item| PathBuf::from(&item.path))
    }

    /// Add a folder and make it the active project. Adding the same folder
    /// again is not an error: it moves to the top and becomes active.
    pub async fn add(&self, path: &Path) -> Result<ProjectList, HostError> {
        if !path.is_absolute() {
            return Err(HostError::new(
                "project_not_absolute",
                format!("a project must be an absolute path: {}", path.display()),
            ));
        }
        if !path.is_dir() {
            return Err(HostError::new(
                "project_not_a_directory",
                format!("not a directory: {}", path.display()),
            ));
        }
        let id = path
            .to_str()
            .ok_or_else(|| {
                HostError::new(
                    "project_path_not_utf8",
                    format!("the folder name is not valid UTF-8: {}", path.display()),
                )
            })?
            .to_string();
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| id.clone());

        let mut inner = self.inner.lock().await;
        inner.guard_writable()?;
        let mut file = inner.file.clone();
        match file.items.iter().position(|item| same_path(&item.id, &id)) {
            // Already known: reuse the stored spelling of the path.
            Some(index) => {
                let item = file.items.remove(index);
                file.items.insert(0, item);
            }
            None => file.items.insert(
                0,
                Stored {
                    id: id.clone(),
                    name,
                    path: id.clone(),
                    added_at: crate::state::now(),
                },
            ),
        }
        file.active_id = Some(id);
        self.commit(&mut inner, file).await
    }

    /// Make a stored project the active one.
    pub async fn select(&self, id: &str) -> Result<ProjectList, HostError> {
        let mut inner = self.inner.lock().await;
        inner.guard_writable()?;
        let mut file = inner.file.clone();
        let found = file
            .items
            .iter()
            .find(|item| same_path(&item.id, id))
            .map(|item| item.id.clone());
        let Some(found) = found else {
            return Err(HostError::new(
                "project_not_found",
                format!("no project with id `{id}`"),
            ));
        };
        file.active_id = Some(found);
        self.commit(&mut inner, file).await
    }

    /// Forget a project. The folder itself is untouched.
    pub async fn remove(&self, id: &str) -> Result<ProjectList, HostError> {
        let mut inner = self.inner.lock().await;
        inner.guard_writable()?;
        let mut file = inner.file.clone();
        let before = file.items.len();
        file.items.retain(|item| !same_path(&item.id, id));
        if file.items.len() == before {
            return Err(HostError::new(
                "project_not_found",
                format!("no project with id `{id}`"),
            ));
        }
        // The active project just left; fall back to whatever is now first.
        if file
            .active_id
            .as_deref()
            .is_some_and(|active| same_path(active, id))
        {
            file.active_id = file.items.first().map(|item| item.id.clone());
        }
        self.commit(&mut inner, file).await
    }

    /// Persist, then adopt. A failed write leaves the in-memory list as it was,
    /// so the UI never shows a change that disk did not take.
    async fn commit(&self, inner: &mut Inner, file: File) -> Result<ProjectList, HostError> {
        write(&self.path, &file)?;
        inner.file = file;
        Ok(inner.snapshot())
    }
}

/// Windows compares paths case-insensitively and the picker hands back the
/// user's casing, which need not match what is on file from last time.
fn same_path(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.to_lowercase() == right.to_lowercase()
    } else {
        left == right
    }
}

/// Read the store. A missing file is the normal first run; anything else leaves
/// a reason and an empty list.
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
            "projects_encode_failed",
            format!("could not encode the project list: {error}"),
        )
    })?;
    std::fs::write(path, format!("{text}\n")).map_err(|error| {
        HostError::new(
            "projects_write_failed",
            format!("could not write {}: {error}", path.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh store in its own directory, plus a folder to add to it.
    fn scratch(tag: &str) -> (ProjectsStore, PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("rust-agent-projects-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("store");
        let project = root.join("my-app");
        std::fs::create_dir_all(&project).unwrap();
        (ProjectsStore::open(&dir).unwrap(), dir, project)
    }

    #[tokio::test]
    async fn adding_a_folder_keeps_it_and_makes_it_active() {
        let (store, _dir, project) = scratch("add");

        let list = store.add(&project).await.unwrap();
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].name, "my-app");
        assert_eq!(list.items[0].path, project.to_str().unwrap());
        assert!(list.items[0].exists);
        assert!(list.warnings.is_empty());
        assert_eq!(list.active_id.as_deref(), project.to_str());
        assert_eq!(store.active_path().await.unwrap(), project);
    }

    #[tokio::test]
    async fn a_folder_is_stored_once_however_it_is_spelled() {
        let (store, _dir, project) = scratch("dupe");
        store.add(&project).await.unwrap();

        // Windows resolves the same folder through any casing; the picker hands
        // back whatever the user typed, which need not match last time.
        let other = if cfg!(windows) {
            PathBuf::from(project.to_str().unwrap().to_uppercase())
        } else {
            project.clone()
        };
        let list = store.add(&other).await.unwrap();

        assert_eq!(list.items.len(), 1, "{list:?}");
        // The stored spelling wins, so the row cannot flicker between cases.
        assert_eq!(list.items[0].path, project.to_str().unwrap());
    }

    #[tokio::test]
    async fn a_missing_folder_is_flagged_and_not_dropped() {
        let (store, _dir, project) = scratch("missing");
        store.add(&project).await.unwrap();
        std::fs::remove_dir_all(&project).unwrap();

        let list = store.snapshot().await;
        assert_eq!(list.items.len(), 1);
        assert!(!list.items[0].exists);
    }

    #[tokio::test]
    async fn removing_the_active_project_falls_back_to_the_next_one() {
        let (store, _dir, project) = scratch("remove");
        let second = project.parent().unwrap().join("second-app");
        std::fs::create_dir_all(&second).unwrap();
        store.add(&project).await.unwrap();
        store.add(&second).await.unwrap();

        let list = store.remove(project.to_str().unwrap()).await.unwrap();
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].path, second.to_str().unwrap());
        assert_eq!(list.active_id.as_deref(), second.to_str());
        assert_eq!(store.active_path().await.unwrap(), second);

        let empty = store.remove(second.to_str().unwrap()).await.unwrap();
        assert!(empty.items.is_empty());
        assert!(empty.active_id.is_none());
        assert!(store.active_path().await.is_none());
    }

    #[tokio::test]
    async fn an_unknown_id_is_an_error_rather_than_a_silent_no_op() {
        let (store, _dir, _project) = scratch("unknown");

        assert_eq!(
            store.select("nope").await.unwrap_err().code,
            "project_not_found"
        );
        assert_eq!(
            store.remove("nope").await.unwrap_err().code,
            "project_not_found"
        );
        assert_eq!(
            store.add(Path::new("relative/dir")).await.unwrap_err().code,
            "project_not_absolute"
        );
    }

    #[tokio::test]
    async fn the_list_survives_a_reopen() {
        let (store, dir, project) = scratch("reopen");
        store.add(&project).await.unwrap();

        let list = ProjectsStore::open(&dir).unwrap().snapshot().await;
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].path, project.to_str().unwrap());
        assert_eq!(list.active_id.as_deref(), project.to_str());
    }

    #[tokio::test]
    async fn the_stored_file_is_plain_json_keyed_for_the_frontend() {
        let (store, dir, project) = scratch("shape");
        store.add(&project).await.unwrap();

        let text = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["version"], serde_json::json!(1));
        assert_eq!(
            value["activeId"],
            serde_json::json!(project.to_str().unwrap())
        );
        assert!(value["items"][0]["addedAt"].is_string());
    }

    #[tokio::test]
    async fn a_corrupt_store_is_reported_and_left_alone() {
        let (store, dir, project) = scratch("corrupt");
        store.add(&project).await.unwrap();
        let file = dir.join(FILE_NAME);
        std::fs::write(&file, "{ not json").unwrap();

        let reopened = ProjectsStore::open(&dir).unwrap();
        let list = reopened.snapshot().await;
        assert!(list.items.is_empty());
        assert_eq!(list.warnings.len(), 1);

        let error = reopened.add(&project).await.unwrap_err();
        assert_eq!(error.code, "projects_store_unreadable");
        // Left alone, not repaired: the bytes are still there for a person to
        // look at, and the list is not silently replaced with an empty one.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{ not json");
    }

    #[tokio::test]
    async fn a_future_version_is_refused_rather_than_guessed() {
        let (_store, dir, _project) = scratch("version");
        std::fs::write(
            dir.join(FILE_NAME),
            r#"{"version":99,"activeId":null,"items":[]}"#,
        )
        .unwrap();

        let reopened = ProjectsStore::open(&dir).unwrap();
        let list = reopened.snapshot().await;
        assert_eq!(list.warnings.len(), 1);
        assert!(list.warnings[0].contains("version 99"), "{list:?}");
    }
}
