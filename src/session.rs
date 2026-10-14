//! Durable conversations.
//!
//! The loop keeps a conversation in memory for the life of the process: quit,
//! and it is gone. A front end that restarts the runtime -- choosing another
//! project does exactly that -- starts from nothing, and the transcript the user
//! was reading only ever existed in the window that drew it. This module is the
//! file that outlives the process.
//!
//! One conversation is one JSON file under `<workspace>/.sessions`, holding the
//! messages the model saw plus whatever else its writer wants back. The runtime
//! stores that second part verbatim and never reads inside it, so a front end
//! can restore the transcript it drew instead of rebuilding one that merely
//! resembles it.
//!
//! The store owns path safety the way [`MemoryStore`](crate::MemoryStore) does:
//! an id is a bare, lower-case file name resolved inside the store directory, so
//! an id that arrives from a front end can never name a file outside it.

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::message::Message;

/// Directory (under the workspace) that holds the conversations.
pub const SESSIONS_DIR: &str = ".sessions";

/// How many conversations are kept before the oldest are dropped.
///
/// A conversation can be long, so the bound is on how many are kept rather than
/// on how big one may be: twenty conversations is more history than a workbench
/// needs, and truncating a long one would lose the part that matters.
pub const DEFAULT_MAX_SESSIONS: usize = 20;

/// Longest id the store accepts, in bytes.
const MAX_ID_BYTES: usize = 64;

/// What a file is called while it is being written, before it is renamed.
const TEMP_SUFFIX: &str = "tmp";

/// Counter behind [`new_session_id`], so two ids in the same millisecond differ.
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// Whether `id` may name a conversation.
///
/// Ids are file names, so this is the store's safety boundary: lower-case ASCII
/// letters, digits and dashes only. That admits no `.`, no path separator and no
/// case-folding surprise, so an id from anywhere cannot escape the store
/// directory or collide with another id's file.
pub fn is_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A fresh session id: the clock in milliseconds, the process, and a counter.
pub fn new_session_id() -> String {
    let millis = Utc::now().timestamp_millis();
    let counter = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("s{millis}-{}-{counter}", std::process::id())
}

/// One conversation, as it is written to disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// The id, which is also the file name.
    pub id: String,
    /// A label for a list of conversations; empty when nobody set one.
    #[serde(default)]
    pub title: String,
    /// RFC 3339, when the conversation started.
    pub created_at: String,
    /// RFC 3339, when it was last written.
    pub updated_at: String,
    /// What the model has seen, and what it will see again when the
    /// conversation is picked up.
    pub messages: Vec<Message>,
    /// The writer's own record of the conversation: the events it drew, the ids
    /// it handed out. Stored and returned verbatim; the runtime never reads it.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub view: Value,
}

/// One conversation without the conversation behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    /// How many messages the conversation holds.
    pub messages: usize,
}

impl SessionSummary {
    fn of(session: &Session) -> Self {
        SessionSummary {
            id: session.id.clone(),
            title: session.title.clone(),
            created_at: session.created_at.clone(),
            updated_at: session.updated_at.clone(),
            messages: session.messages.len(),
        }
    }
}

/// The file-backed store of conversations.
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
    max: usize,
}

impl SessionStore {
    /// A store at `<workspace>/.sessions`.
    pub fn new(workspace: impl AsRef<Path>) -> Self {
        SessionStore::at(workspace.as_ref().join(SESSIONS_DIR))
    }

    /// A store rooted at an explicit directory (mainly for tests).
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        SessionStore {
            dir: dir.into(),
            max: DEFAULT_MAX_SESSIONS,
        }
    }

    /// Keep at most this many conversations.
    pub fn with_max(mut self, max: usize) -> Self {
        self.max = max.max(1);
        self
    }

    /// The store directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Resolve an id to a path inside the store.
    fn resolve(&self, id: &str) -> Result<PathBuf> {
        if !is_session_id(id) {
            return Err(Error::Session(format!("invalid session id `{id}`")));
        }
        Ok(self.dir.join(format!("{id}.json")))
    }

    /// A new conversation, in memory.
    ///
    /// Nothing is written until it is saved: an empty conversation is not worth
    /// a file, and a window that never sent a prompt should not leave one
    /// behind.
    pub fn create(&self, title: impl Into<String>) -> Session {
        let now = Utc::now().to_rfc3339();
        Session {
            id: new_session_id(),
            title: title.into(),
            created_at: now.clone(),
            updated_at: now,
            messages: Vec::new(),
            view: Value::Null,
        }
    }

    /// Write one conversation, stamp when, and drop the oldest beyond the cap.
    pub fn save(&self, session: &mut Session) -> Result<PathBuf> {
        session.updated_at = Utc::now().to_rfc3339();
        let path = self.resolve(&session.id)?;
        fs::create_dir_all(&self.dir)?;
        let text = serde_json::to_string_pretty(session)?;
        // Write beside the file and rename over it, so a process that dies
        // mid-write leaves the previous conversation whole rather than a
        // truncated one.
        let temp = self.dir.join(format!("{}.{TEMP_SUFFIX}", session.id));
        {
            let mut file = fs::File::create(&temp)?;
            file.write_all(text.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
        }
        fs::rename(&temp, &path)?;
        self.prune(&session.id)?;
        Ok(path)
    }

    /// Read one conversation.
    pub fn load(&self, id: &str) -> Result<Session> {
        let path = self.resolve(id)?;
        if !path.is_file() {
            return Err(Error::Session(format!("no session `{id}`")));
        }
        Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
    }

    /// Every conversation, most recently written first.
    ///
    /// A file this build cannot read is left exactly where it is and simply not
    /// listed: one damaged conversation must not hide the others, and it is not
    /// this build's place to delete what it cannot understand.
    pub fn list(&self) -> Result<Vec<SessionSummary>> {
        if !self.dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut summaries = Vec::new();
        for path in fs::read_dir(&self.dir)?.flatten().map(|entry| entry.path()) {
            if !path.is_file() || path.extension() != Some(OsStr::new("json")) {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(session) = serde_json::from_str::<Session>(&text) else {
                continue;
            };
            summaries.push(SessionSummary::of(&session));
        }
        summaries.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(summaries)
    }

    /// The conversation written most recently, when the store holds one.
    pub fn latest(&self) -> Result<Option<Session>> {
        match self.list()?.into_iter().next() {
            Some(summary) => Ok(Some(self.load(&summary.id)?)),
            None => Ok(None),
        }
    }

    /// Delete one conversation. `false` when it was not there.
    pub fn delete(&self, id: &str) -> Result<bool> {
        let path = self.resolve(id)?;
        if !path.is_file() {
            return Ok(false);
        }
        fs::remove_file(path)?;
        Ok(true)
    }

    /// Keep the newest `max` conversations and drop the rest.
    ///
    /// Only ever called after a save, and never for the conversation that was
    /// just saved, so a store being read is not emptied under the reader.
    fn prune(&self, keep: &str) -> Result<()> {
        let summaries = self.list()?;
        if summaries.len() <= self.max {
            return Ok(());
        }
        for summary in summaries.iter().skip(self.max) {
            if summary.id == keep {
                continue;
            }
            let path = self.resolve(&summary.id)?;
            if path.is_file() {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store capped at three conversations, so pruning is cheap to test.
    fn store() -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::at(dir.path()).with_max(3);
        (dir, store)
    }

    fn conversation(store: &SessionStore, title: &str, messages: usize) -> Session {
        let mut session = store.create(title);
        for index in 0..messages {
            session
                .messages
                .push(Message::user_text(format!("m{index}")));
        }
        store.save(&mut session).unwrap();
        session
    }

    #[test]
    fn a_conversation_round_trips_through_the_store() {
        let (_dir, store) = store();
        let mut session = conversation(&store, "first", 2);
        session.view = serde_json::json!({ "turns": [{ "turn": "1" }] });
        store.save(&mut session).unwrap();

        let read = store.load(&session.id).unwrap();
        assert_eq!(read, session);
        assert_eq!(read.messages.len(), 2);
        assert!(!read.updated_at.is_empty());
    }

    #[test]
    fn an_unsaved_conversation_leaves_no_file_behind() {
        let (_dir, store) = store();
        let session = store.create("nothing said yet");
        assert_eq!(fs::read_dir(store.dir()).unwrap().count(), 0);
        assert!(store.list().unwrap().is_empty());
        assert!(store.load(&session.id).is_err());
    }

    #[test]
    fn a_fresh_id_is_unique_and_names_a_file() {
        let first = new_session_id();
        let second = new_session_id();
        assert_ne!(first, second);
        assert!(is_session_id(&first), "{first} should be an id");
    }

    #[test]
    fn an_id_that_could_name_another_file_is_refused() {
        let (_dir, store) = store();
        for id in [
            "",
            "..",
            "../escape",
            "sub/dir",
            "sess.json",
            "A-P-C",
            "s1 2",
        ] {
            assert!(!is_session_id(id), "`{id}` should not be an id");
            assert!(store.load(id).is_err(), "`{id}` should not load");
            assert!(store.delete(id).is_err(), "`{id}` should not delete");
        }
    }

    #[test]
    fn the_list_is_newest_first_and_counts_messages() {
        let (_dir, store) = store();
        let first = conversation(&store, "first", 1);
        let second = conversation(&store, "second", 3);

        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, second.id);
        assert_eq!(list[0].title, "second");
        assert_eq!(list[0].messages, 3);
        assert_eq!(list[1].id, first.id);
    }

    #[test]
    fn a_file_this_build_cannot_read_is_kept_and_not_listed() {
        let (_dir, store) = store();
        let good = conversation(&store, "good", 1);
        fs::write(store.dir().join("broken.json"), "{ not json").unwrap();

        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, good.id);
        assert!(store.dir().join("broken.json").is_file());
    }

    #[test]
    fn the_oldest_conversations_beyond_the_cap_are_dropped() {
        let (_dir, store) = store();
        let oldest = conversation(&store, "oldest", 1);
        let second = conversation(&store, "second", 1);
        let third = conversation(&store, "third", 1);
        let fourth = conversation(&store, "fourth", 1);

        let ids: Vec<String> = store.list().unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, [fourth.id.clone(), third.id, second.id]);
        assert!(!store.dir().join(format!("{}.json", oldest.id)).exists());
    }

    #[test]
    fn the_latest_conversation_is_the_one_written_last() {
        let (_dir, store) = store();
        assert!(store.latest().unwrap().is_none());
        conversation(&store, "older", 1);
        let newest = conversation(&store, "newest", 1);
        assert_eq!(store.latest().unwrap().unwrap().id, newest.id);
    }

    #[test]
    fn deleting_a_conversation_removes_its_file_and_nothing_else() {
        let (_dir, store) = store();
        let gone = conversation(&store, "gone", 1);
        let kept = conversation(&store, "kept", 1);

        assert!(store.delete(&gone.id).unwrap());
        assert!(!store.delete(&gone.id).unwrap());
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(store.load(&kept.id).is_ok());
    }

    #[test]
    fn a_half_written_file_is_not_a_conversation() {
        let (_dir, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.dir().join("s1-2-3.tmp"), "{\"id\":\"s1-2-3\"").unwrap();
        assert!(store.list().unwrap().is_empty());
    }
}
