//! The storage calls the runtime makes, as a trait.
//!
//! The TypeScript reached SQLite through Tauri `invoke` commands. Here those
//! commands are the methods of `SessionBackend` and `CheckpointBackend`.
//! `StoreBackend` implements both over `monocode-store`, running each
//! blocking call on GPUI's background executor. Tests use the fake in
//! `runtime::testing`, the way the TypeScript tests mocked `invoke`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;
use gpui::BackgroundExecutor;
use monocode_store::StoreEvents;
use monocode_store::checkpoint::{
    self, CheckpointApplyResult, CheckpointFileDiff, CheckpointStatus, CheckpointStore,
};
use monocode_store::cli_sessions::{CliSession, Entry as CliEntry};
use monocode_store::context_history::{self, ContextAssetSnapshot, ContextAssetSource};
use monocode_store::session_store::{
    self, InFlightSession, SessionRecord, SessionSearchOptions, SessionSearchResult, SessionStore,
    SessionSummary as StoredSummary, SessionUpsert,
};
use serde_json::Value;

/// One storage call in flight. Errors are the store's message strings.
pub type StoreFuture<T> = BoxFuture<'static, Result<T, String>>;

/// The session commands of src-tauri, one method per `invoke` name.
pub trait SessionBackend: Send + Sync + 'static {
    /// `session_upsert`.
    fn upsert(&self, session: SessionUpsert) -> StoreFuture<StoredSummary>;
    /// `session_get`.
    fn get(&self, session_id: String) -> StoreFuture<Option<SessionRecord>>;
    /// `session_list_by_project`.
    fn list_by_project(&self, cwd: String) -> StoreFuture<Vec<StoredSummary>>;
    /// `session_rebase_project`.
    fn rebase_project(&self, from_cwd: String, to_cwd: String) -> StoreFuture<()>;
    /// `session_list_linked`.
    fn list_linked(&self) -> StoreFuture<Vec<StoredSummary>>;
    /// `session_search`.
    fn search(&self, options: SessionSearchOptions) -> StoreFuture<SessionSearchResult>;
    /// `cancel_session_search`.
    fn cancel_search(&self, search_owner: String) -> StoreFuture<()>;
    /// `session_delete`.
    fn delete(&self, session_id: String, image_paths: Vec<String>) -> StoreFuture<()>;
    /// `session_discard_draft`: drop a transient draft record. The id stays
    /// usable, and settled history or provider execution is refused.
    fn discard_draft(&self, session_id: String) -> StoreFuture<()>;
    /// `session_set_archived`.
    fn set_archived(&self, session_id: String, archived: bool) -> StoreFuture<()>;
    /// `session_set_pinned`.
    fn set_pinned(&self, session_id: String, pinned: bool) -> StoreFuture<()>;
    /// `session_set_linked_work_item`. `None` removes the link.
    fn set_linked_work_item(&self, session_id: String, item: Option<Value>) -> StoreFuture<()>;
    /// `session_set_in_flight`.
    fn set_in_flight(&self, sessions: Vec<InFlightSession>) -> StoreFuture<()>;
    /// `session_list_in_flight`: read the quit snapshot without clearing it.
    fn list_in_flight(&self) -> StoreFuture<Vec<InFlightSession>>;
    /// `session_take_in_flight`: read and clear the quit snapshot.
    fn take_in_flight(&self) -> StoreFuture<Vec<InFlightSession>>;
    /// `workspace_set_snapshot`.
    fn set_workspace_snapshot(&self, snapshot: Value) -> StoreFuture<()>;
    /// `workspace_get_snapshot`.
    fn workspace_snapshot(&self) -> StoreFuture<Option<Value>>;
    /// Every link between two sessions, each pair once.
    fn list_session_links(&self) -> StoreFuture<Vec<(String, String)>>;
    /// Add (`linked`) or remove the link between two sessions.
    fn set_session_link(&self, a: String, b: String, linked: bool) -> StoreFuture<()>;
    /// Save a portable transcript snapshot of `session_id` as `name` and
    /// return its absolute path, for an agent's file tools to read.
    fn write_context_snapshot(
        &self,
        session_id: String,
        name: String,
        text: String,
    ) -> StoreFuture<String>;
    /// `claude_shell_commands`: Bash commands by tool-use id, read from
    /// Claude's own transcript.
    fn claude_shell_commands(
        &self,
        provider_session_id: String,
        provider_account_id: Option<String>,
        tool_ids: Vec<String>,
    ) -> StoreFuture<HashMap<String, String>>;
    /// `session_context_snapshot`: save the shared history of one provider
    /// switch and return its path. The same content returns the same path.
    fn write_switch_snapshot(
        &self,
        _session_id: String,
        _switch_id: String,
        _content: String,
    ) -> StoreFuture<String> {
        unsupported("Saving shared history")
    }
    /// `session_context_assets`: durable copies of historical attachments.
    fn snapshot_context_assets(
        &self,
        _session_id: String,
        _attachments: Vec<ContextAssetSource>,
    ) -> StoreFuture<Vec<ContextAssetSnapshot>> {
        unsupported("Saving historical attachments")
    }
    /// `cli_sessions_list`: sessions the provider CLIs recorded for `cwd`
    /// that have no row yet.
    fn cli_sessions_list(&self, _cwd: String) -> StoreFuture<Vec<CliSession>> {
        unsupported("Listing CLI sessions")
    }
    /// `cli_session_read`: one CLI transcript as import entries.
    fn cli_session_read(&self, _harness: String, _path: PathBuf) -> StoreFuture<Vec<CliEntry>> {
        unsupported("Reading CLI sessions")
    }
    /// `session_import`: save an imported session with the CLI's own
    /// timestamps. `None` when that provider session already has a row.
    fn import_session(
        &self,
        _session: SessionUpsert,
        _created_at: i64,
        _updated_at: i64,
    ) -> StoreFuture<Option<StoredSummary>> {
        unsupported("Importing CLI sessions")
    }
}

fn unsupported<T: Send + 'static>(what: &str) -> StoreFuture<T> {
    futures::future::ready(Err(format!("{what} is not available here"))).boxed()
}

/// The `session_checkpoint_*` commands.
pub trait CheckpointBackend: Send + Sync + 'static {
    /// `isolated` marks a worker that owns its checkout, so every later
    /// change there counts as its own.
    fn ensure(&self, session_id: String, cwd: String, isolated: bool) -> StoreFuture<()>;
    fn prepare(&self, session_id: String, cwd: String, paths: Vec<String>) -> StoreFuture<()>;
    fn capture(&self, session_id: String, cwd: String, paths: Vec<String>) -> StoreFuture<()>;
    fn status(&self, session_id: String, cwd: String) -> StoreFuture<CheckpointStatus>;
    fn apply(
        &self,
        session_id: String,
        from_cwd: String,
        to_cwd: String,
        write_scopes: Option<Vec<String>>,
    ) -> StoreFuture<CheckpointApplyResult>;
    fn cleanup_safe(&self, session_id: String, cwd: String) -> StoreFuture<bool>;
    fn forget(&self, session_id: String) -> StoreFuture<()>;
    fn file_diff(
        &self,
        session_id: String,
        cwd: String,
        relative: String,
    ) -> StoreFuture<CheckpointFileDiff>;
    fn undo(
        &self,
        session_id: String,
        cwd: String,
        relative: Option<String>,
    ) -> StoreFuture<CheckpointStatus>;
    fn keep(
        &self,
        session_id: String,
        cwd: String,
        relative: Option<String>,
    ) -> StoreFuture<CheckpointStatus>;
}

/// Write `<dir>/<session_id>/<name>.md`. Both parts must be plain ids so
/// the file stays inside `dir`.
pub fn write_context_snapshot(
    dir: &std::path::Path,
    session_id: &str,
    name: &str,
    text: &str,
) -> Result<String, String> {
    session_store::validate_id(session_id, "session")?;
    session_store::validate_id(name, "snapshot")?;
    let folder = dir.join(session_id);
    std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    let path = folder.join(format!("{name}.md"));
    std::fs::write(&path, text).map_err(|error| error.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

/// Store change notices with nobody listening.
pub struct NoStoreEvents;

impl StoreEvents for NoStoreEvents {
    fn reminders_changed(&self) {}
    fn automations_changed(&self) {}
}

/// `SessionBackend` and `CheckpointBackend` over `monocode-store`.
#[derive(Clone)]
pub struct StoreBackend {
    sessions: Arc<SessionStore>,
    checkpoints: Option<Arc<CheckpointStore>>,
    data_dir: PathBuf,
    events: Arc<dyn StoreEvents>,
    executor: BackgroundExecutor,
}

impl StoreBackend {
    pub fn new(
        sessions: Arc<SessionStore>,
        checkpoints: Option<Arc<CheckpointStore>>,
        data_dir: PathBuf,
        events: Arc<dyn StoreEvents>,
        executor: BackgroundExecutor,
    ) -> Self {
        Self {
            sessions,
            checkpoints,
            data_dir,
            events,
            executor,
        }
    }

    /// Open `monocode.db` and the checkpoint store in the app data directory.
    pub fn open(data_dir: PathBuf, executor: BackgroundExecutor) -> Result<Self, String> {
        let sessions = Arc::new(session_store::open_in_data_dir(&data_dir)?);
        let checkpoints = Some(Arc::new(checkpoint::init(&data_dir)?));
        Ok(Self::new(
            sessions,
            checkpoints,
            data_dir,
            Arc::new(NoStoreEvents),
            executor,
        ))
    }

    /// An in-memory database with the full schema, for tests.
    pub fn in_memory(executor: BackgroundExecutor) -> Result<Self, String> {
        Ok(Self::new(
            Arc::new(SessionStore::open_in_memory()?),
            None,
            std::env::temp_dir(),
            Arc::new(NoStoreEvents),
            executor,
        ))
    }

    pub fn session_store(&self) -> &Arc<SessionStore> {
        &self.sessions
    }

    fn run<T: Send + 'static>(
        &self,
        op: impl FnOnce(&SessionStore) -> Result<T, String> + Send + 'static,
    ) -> StoreFuture<T> {
        let store = self.sessions.clone();
        self.executor.spawn(async move { op(&store) }).boxed()
    }

    fn checkpoint<T: Send + 'static>(
        &self,
        op: impl FnOnce(&CheckpointStore) -> Result<T, String> + Send + 'static,
    ) -> StoreFuture<T> {
        let Some(store) = self.checkpoints.clone() else {
            return futures::future::ready(Err("Checkpoint store is not open".to_string())).boxed();
        };
        self.executor.spawn(async move { op(&store) }).boxed()
    }
}

impl SessionBackend for StoreBackend {
    fn upsert(&self, session: SessionUpsert) -> StoreFuture<StoredSummary> {
        self.run(move |store| session_store::session_upsert(store, session))
    }

    fn cli_sessions_list(&self, cwd: String) -> StoreFuture<Vec<CliSession>> {
        self.run(move |store| session_store::cli_sessions_list(store, &cwd))
    }

    fn cli_session_read(&self, harness: String, path: PathBuf) -> StoreFuture<Vec<CliEntry>> {
        self.run(move |_| session_store::cli_session_read(&harness, &path))
    }

    fn import_session(
        &self,
        session: SessionUpsert,
        created_at: i64,
        updated_at: i64,
    ) -> StoreFuture<Option<StoredSummary>> {
        self.run(move |store| session_store::session_import(store, session, created_at, updated_at))
    }

    fn get(&self, session_id: String) -> StoreFuture<Option<SessionRecord>> {
        self.run(move |store| session_store::session_get(store, session_id))
    }

    fn list_by_project(&self, cwd: String) -> StoreFuture<Vec<StoredSummary>> {
        self.run(move |store| session_store::session_list_by_project(store, cwd))
    }

    fn rebase_project(&self, from_cwd: String, to_cwd: String) -> StoreFuture<()> {
        self.run(move |store| session_store::session_rebase_project(store, from_cwd, to_cwd))
    }

    fn list_linked(&self) -> StoreFuture<Vec<StoredSummary>> {
        self.run(session_store::session_list_linked)
    }

    fn search(&self, options: SessionSearchOptions) -> StoreFuture<SessionSearchResult> {
        self.run(move |store| session_store::session_search(store, options))
    }

    fn cancel_search(&self, search_owner: String) -> StoreFuture<()> {
        session_store::cancel_session_search(search_owner);
        futures::future::ready(Ok(())).boxed()
    }

    fn delete(&self, session_id: String, image_paths: Vec<String>) -> StoreFuture<()> {
        let data_dir = self.data_dir.clone();
        let events = self.events.clone();
        self.run(move |store| {
            session_store::session_delete(
                store,
                &data_dir,
                events.as_ref(),
                session_id,
                image_paths,
            )
        })
    }

    fn discard_draft(&self, session_id: String) -> StoreFuture<()> {
        let events = self.events.clone();
        self.run(move |store| {
            session_store::session_discard_draft(store, events.as_ref(), session_id)
        })
    }

    fn set_archived(&self, session_id: String, archived: bool) -> StoreFuture<()> {
        self.run(move |store| session_store::session_set_archived(store, session_id, archived))
    }

    fn set_pinned(&self, session_id: String, pinned: bool) -> StoreFuture<()> {
        self.run(move |store| session_store::session_set_pinned(store, session_id, pinned))
    }

    fn set_linked_work_item(&self, session_id: String, item: Option<Value>) -> StoreFuture<()> {
        self.run(move |store| session_store::session_set_linked_work_item(store, session_id, item))
    }

    fn set_in_flight(&self, sessions: Vec<InFlightSession>) -> StoreFuture<()> {
        self.run(move |store| session_store::session_set_in_flight(store, sessions))
    }

    fn list_in_flight(&self) -> StoreFuture<Vec<InFlightSession>> {
        self.run(session_store::session_list_in_flight)
    }

    fn take_in_flight(&self) -> StoreFuture<Vec<InFlightSession>> {
        self.run(session_store::session_take_in_flight)
    }

    fn set_workspace_snapshot(&self, snapshot: Value) -> StoreFuture<()> {
        self.run(move |store| session_store::workspace_set_snapshot(store, snapshot))
    }

    fn workspace_snapshot(&self) -> StoreFuture<Option<Value>> {
        self.run(session_store::workspace_get_snapshot)
    }

    fn list_session_links(&self) -> StoreFuture<Vec<(String, String)>> {
        self.run(monocode_store::session_links::session_list_links)
    }

    fn set_session_link(&self, a: String, b: String, linked: bool) -> StoreFuture<()> {
        self.run(move |store| monocode_store::session_links::session_set_link(store, a, b, linked))
    }

    fn write_context_snapshot(
        &self,
        session_id: String,
        name: String,
        text: String,
    ) -> StoreFuture<String> {
        let dir = self.data_dir.join("context-snapshots");
        self.executor
            .spawn(async move { write_context_snapshot(&dir, &session_id, &name, &text) })
            .boxed()
    }

    fn write_switch_snapshot(
        &self,
        session_id: String,
        switch_id: String,
        content: String,
    ) -> StoreFuture<String> {
        let data_dir = self.data_dir.clone();
        self.run(move |store| {
            context_history::session_context_snapshot(
                store,
                &data_dir,
                &session_id,
                &switch_id,
                &content,
            )
        })
    }

    fn snapshot_context_assets(
        &self,
        session_id: String,
        attachments: Vec<ContextAssetSource>,
    ) -> StoreFuture<Vec<ContextAssetSnapshot>> {
        let data_dir = self.data_dir.clone();
        self.run(move |store| {
            context_history::session_context_assets(store, &data_dir, &session_id, attachments)
        })
    }

    fn claude_shell_commands(
        &self,
        provider_session_id: String,
        provider_account_id: Option<String>,
        tool_ids: Vec<String>,
    ) -> StoreFuture<HashMap<String, String>> {
        let data_dir = self.data_dir.clone();
        self.executor
            .spawn(async move {
                monocode_git::fs::claude_shell_commands(
                    &data_dir,
                    provider_session_id,
                    provider_account_id,
                    tool_ids,
                )
            })
            .boxed()
    }
}

impl CheckpointBackend for StoreBackend {
    fn ensure(&self, session_id: String, cwd: String, isolated: bool) -> StoreFuture<()> {
        self.checkpoint(move |store| {
            checkpoint::session_checkpoint_ensure(store, session_id, cwd, isolated)
        })
    }

    fn prepare(&self, session_id: String, cwd: String, paths: Vec<String>) -> StoreFuture<()> {
        self.checkpoint(move |store| {
            checkpoint::session_checkpoint_prepare(store, session_id, cwd, paths)
        })
    }

    fn capture(&self, session_id: String, cwd: String, paths: Vec<String>) -> StoreFuture<()> {
        self.checkpoint(move |store| {
            checkpoint::session_checkpoint_capture(store, session_id, cwd, paths)
        })
    }

    fn status(&self, session_id: String, cwd: String) -> StoreFuture<CheckpointStatus> {
        self.checkpoint(move |store| checkpoint::session_checkpoint_status(store, session_id, cwd))
    }

    fn apply(
        &self,
        session_id: String,
        from_cwd: String,
        to_cwd: String,
        write_scopes: Option<Vec<String>>,
    ) -> StoreFuture<CheckpointApplyResult> {
        self.checkpoint(move |store| {
            checkpoint::session_checkpoint_apply(store, session_id, from_cwd, to_cwd, write_scopes)
        })
    }

    fn cleanup_safe(&self, session_id: String, cwd: String) -> StoreFuture<bool> {
        self.checkpoint(move |store| {
            checkpoint::session_checkpoint_cleanup_safe(store, session_id, cwd)
        })
    }

    fn forget(&self, session_id: String) -> StoreFuture<()> {
        self.checkpoint(move |store| checkpoint::session_checkpoint_forget(store, session_id))
    }

    fn file_diff(
        &self,
        session_id: String,
        cwd: String,
        relative: String,
    ) -> StoreFuture<CheckpointFileDiff> {
        self.checkpoint(move |store| {
            checkpoint::session_checkpoint_file_diff(store, session_id, cwd, relative)
        })
    }

    fn undo(
        &self,
        session_id: String,
        cwd: String,
        relative: Option<String>,
    ) -> StoreFuture<CheckpointStatus> {
        self.checkpoint(move |store| {
            checkpoint::session_checkpoint_undo(store, session_id, cwd, relative)
        })
    }

    fn keep(
        &self,
        session_id: String,
        cwd: String,
        relative: Option<String>,
    ) -> StoreFuture<CheckpointStatus> {
        self.checkpoint(move |store| {
            checkpoint::session_checkpoint_keep(store, session_id, cwd, relative)
        })
    }
}
