//! Git status per project folder. Port of the shared stores in
//! src/features/source-control/hooks (useGitFileStatuses,
//! useProjectBranches, useProjectDiffStats, useProjectWorktrees) and the
//! changes panel's diff index poll (`useDiffIndex` in GitChangesPanel.tsx).
//!
//! `GitStatuses` keeps one `GitStatus` entity per folder for the life of
//! the app, as the TypeScript module maps did. A view asks for what it
//! shows with `GitStatus::watch`; the returned `GitWatch` keeps that part
//! loading until it is dropped (the hooks' subscribe and unsubscribe).
//! Every git call runs on the background executor.
//!
//! Refresh triggers match the TypeScript:
//! - Window focus and becoming visible reload what is watched (diff stats
//!   only after `DIFF_STATS_RESUME_TTL_MS`).
//! - `git_changed` (notifyGitChanged) reloads statuses, branches, diff
//!   stats, worktrees, and the index. Calls in one effect cycle coalesce.
//! - `dirs_changed` (notifyDirsChanged) reloads the file statuses.
//! - The diff index polls every `GIT_POLL` while watched and visible.
//!
//! The file statuses and the diff index come from the same `git_diff_index`
//! call, so a folder watched by both runs one call per trigger.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::Shared;
use gpui::{App, AppContext, Context, Entity, Task};
use monocode_core::js;
use monocode_core::paths::{path_key, slash};
use monocode_git::fs::{GitBranches, GitDiffIndex, GitDiffStats};

use super::Clock;
use super::backend::{ProjectsBackend, Worktrees};
use crate::runtime::engine::Engine;

/// `GIT_POLL_MS`: the changes panel's poll.
pub const GIT_POLL: Duration = Duration::from_millis(2000);
/// While the app has windows but none has focus, the poll reads git on
/// every this many ticks (every 10 s) instead of every tick. One read runs
/// about a dozen git processes. Focus coming back reloads at once
/// (`GitStatus::resume`), and `notify_git_changed` still reloads at once.
pub const GIT_POLL_UNFOCUSED_TICKS: u32 = 5;
/// `RESUME_TTL_MS`: diff stats younger than this skip a focus reload.
pub const DIFF_STATS_RESUME_TTL_MS: i64 = 30_000;

/// `GitStatusMap`: each changed file's status, and each parent folder's
/// strongest status.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitStatusMap {
    pub files: HashMap<String, String>,
    pub dirs: HashMap<String, String>,
}

/// `STATUS_PRIORITY`.
fn status_priority(status: &str) -> u8 {
    match status {
        "modified" => 3,
        "deleted" => 2,
        "added" | "untracked" => 1,
        _ => 0,
    }
}

/// `trimSlash`.
fn trim_slash(path: &str) -> String {
    let slashed = slash(path);
    let trimmed = slashed.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".into()
    } else {
        trimmed.to_string()
    }
}

fn is_drive(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// `parentPath` from src/shared/lib/paths.ts.
pub fn parent_path(path: &str) -> String {
    let trimmed = trim_slash(path);
    // `//server/share` is a UNC root.
    if let Some(rest) = trimmed.strip_prefix("//") {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() == 2 && parts.iter().all(|part| !part.is_empty()) {
            return trimmed;
        }
    }
    if is_drive(&trimmed) {
        return format!("{trimmed}/");
    }
    let Some(index) = trimmed.rfind('/').filter(|index| *index > 0) else {
        return "/".into();
    };
    let parent = &trimmed[..index];
    if is_drive(parent) {
        return format!("{parent}/");
    }
    parent.to_string()
}

/// `buildStatusMaps`.
pub fn build_status_maps(index: &GitDiffIndex, cwd: &str) -> GitStatusMap {
    let mut map = GitStatusMap::default();
    let cwd_len = js::len(cwd);
    for file in &index.files {
        map.files.insert(file.path.clone(), file.status.clone());
        let priority = status_priority(&file.status);
        if priority == 0 {
            continue;
        }
        let mut dir = parent_path(&file.path);
        while js::len(&dir) > cwd_len {
            let existing = map.dirs.get(&dir).map(|status| status_priority(status));
            if priority > existing.unwrap_or(0) {
                map.dirs.insert(dir.clone(), file.status.clone());
            } else {
                break;
            }
            dir = parent_path(&dir);
        }
    }
    map
}

/// `sameIndex`: everything but the absolute file paths.
pub fn same_index(prev: Option<&GitDiffIndex>, next: &GitDiffIndex) -> bool {
    let Some(prev) = prev else {
        return false;
    };
    if prev.branch != next.branch
        || prev.head != next.head
        || prev.additions != next.additions
        || prev.deletions != next.deletions
        || prev.files.len() != next.files.len()
        || prev.remote != next.remote
        || prev.upstream != next.upstream
        || prev.default_branch != next.default_branch
        || prev.ahead != next.ahead
        || prev.behind != next.behind
        || prev.ahead_of_default != next.ahead_of_default
        || prev.head_pushed != next.head_pushed
    {
        return false;
    }
    prev.files.iter().zip(&next.files).all(|(file, other)| {
        file.relative == other.relative
            && file.status == other.status
            && file.additions == other.additions
            && file.deletions == other.deletions
            && file.staged == other.staged
            && file.unstaged == other.unstaged
    })
}

/// `changedFilePaths`: files whose row changed, appeared, or went away.
pub fn changed_file_paths(prev: &GitDiffIndex, next: &GitDiffIndex) -> Vec<String> {
    let mut paths = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let previous: HashMap<&str, _> = prev
        .files
        .iter()
        .map(|file| (file.relative.as_str(), file))
        .collect();
    let current: std::collections::HashSet<&str> = next
        .files
        .iter()
        .map(|file| file.relative.as_str())
        .collect();
    for file in &next.files {
        let changed = previous.get(file.relative.as_str()).is_none_or(|before| {
            before.status != file.status
                || before.additions != file.additions
                || before.deletions != file.deletions
                || before.staged != file.staged
                || before.unstaged != file.unstaged
        });
        if changed {
            paths.push(file.path.clone());
            seen.insert(file.path.clone());
        }
    }
    for file in &prev.files {
        if !current.contains(file.relative.as_str()) && !seen.contains(&file.path) {
            paths.push(file.path.clone());
        }
    }
    paths
}

/// Whether two indexes agree on everything a ref decides: the branch, the
/// commit, the remote and upstream, and the counts against them.
fn same_refs(prev: &GitDiffIndex, next: &GitDiffIndex) -> bool {
    prev.branch == next.branch
        && prev.head == next.head
        && prev.remote == next.remote
        && prev.upstream == next.upstream
        && prev.default_branch == next.default_branch
        && prev.ahead == next.ahead
        && prev.behind == next.behind
        && prev.ahead_of_default == next.ahead_of_default
        && prev.head_pushed == next.head_pushed
}

/// A folder's poll read a new diff index in which no ref moved: only
/// working-tree files changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesChange {
    /// The folder whose index changed, spelled as its `GitStatus` is keyed.
    pub source: String,
    /// The same folder with symlinks resolved, when that is known.
    pub source_real: Option<String>,
    /// The folder went from no changes to some, or back. Only then can the
    /// clean or dirty flag of the working copy holding it change: a folder
    /// with changes before and after sits in a dirty working copy both times.
    pub emptiness_changed: bool,
}

/// A folder path for comparing: `~` expanded, no trailing slash, and on
/// macOS and Windows, whose default file systems ignore case, lowercased.
/// Lowercasing can only make two different folders look related, which
/// costs a reload, never a missed one.
fn comparable(path: &str) -> String {
    let key = path_key(&monocode_git::fs::expand_home(path).to_string_lossy());
    if cfg!(any(target_os = "macos", windows)) {
        key.to_lowercase()
    } else {
        key
    }
}

/// `path` is `root` or inside it. Both are `comparable` already.
fn within(path: &str, root: &str) -> bool {
    path == root
        || root == "/"
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/') || root.ends_with('/'))
}

/// The spellings of a folder to compare: as given, and with symlinks
/// resolved when known.
fn spellings(raw: &str, real: Option<&str>) -> Vec<String> {
    let mut out = vec![comparable(raw)];
    if let Some(real) = real.map(comparable)
        && !out.contains(&real)
    {
        out.push(real);
    }
    out
}

/// A value that moves whenever a ref, `HEAD`, or a working copy of the
/// repository holding `cwd` changes, from file stamps alone. Git writes a ref
/// through a lock file it renames into place, so every ref write changes
/// the stamp of the directory holding it; this hashes the stamps of
/// `packed-refs`, `HEAD`, every directory under `refs/`, `reftable/`, and
/// the per-working-copy directories under `worktrees/`. `None` when the
/// repository cannot be found or is too large to stamp cheaply.
pub fn refs_fingerprint_on_disk(cwd: &str) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    use std::path::{Path, PathBuf};

    const MAX_ENTRIES: usize = 4_000;

    fn stamp(path: &Path, hasher: &mut impl Hasher) {
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                meta.len().hash(hasher);
                meta.modified().ok().hash(hasher);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    meta.ino().hash(hasher);
                }
            }
            Err(_) => 0u8.hash(hasher),
        }
    }

    /// Stamp `dir` and every directory below it. `false` past the limit.
    fn stamp_tree(dir: &Path, hasher: &mut impl Hasher, budget: &mut usize) -> bool {
        stamp(dir, hasher);
        let Ok(entries) = std::fs::read_dir(dir) else {
            return true;
        };
        let mut dirs: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            if *budget == 0 {
                return false;
            }
            *budget -= 1;
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                dirs.push(entry.path());
            }
        }
        dirs.sort();
        dirs.iter().all(|dir| stamp_tree(dir, hasher, budget))
    }

    /// The working copy's git directory and the repository's common one.
    fn git_dirs(root: &Path) -> Option<(PathBuf, PathBuf)> {
        let mut dir = Some(root);
        while let Some(current) = dir {
            let dot_git = current.join(".git");
            if dot_git.is_dir() {
                return Some((dot_git.clone(), dot_git));
            }
            if dot_git.is_file() {
                let text = std::fs::read_to_string(&dot_git).ok()?;
                let target = text.trim().strip_prefix("gitdir:")?.trim();
                let git_dir = current.join(target);
                let common = std::fs::read_to_string(git_dir.join("commondir"))
                    .ok()
                    .map(|common| git_dir.join(common.trim()))
                    .unwrap_or_else(|| git_dir.clone());
                return Some((git_dir, common));
            }
            dir = current.parent();
        }
        None
    }

    let root = monocode_git::fs::expand_home(cwd);
    let (git_dir, common) = git_dirs(&root)?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut budget = MAX_ENTRIES;
    stamp(&git_dir.join("HEAD"), &mut hasher);
    stamp(&common.join("HEAD"), &mut hasher);
    stamp(&common.join("packed-refs"), &mut hasher);
    for tree in ["refs", "reftable", "worktrees"] {
        if !stamp_tree(&common.join(tree), &mut hasher, &mut budget) {
            return None;
        }
    }
    Some(hasher.finish())
}

/// `ProjectBranchesState`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectBranchesState {
    pub branches: Option<GitBranches>,
    /// The first lookup for this folder finished, repo or not.
    pub settled: bool,
}

/// The `useProjectWorktrees` snapshot: the last good list, and the last
/// error beside it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreesSnapshot {
    pub data: Option<Worktrees>,
    pub error: Option<String>,
}

/// The parts of a folder's git status a view can watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WatchKind {
    /// `useGitFileStatuses`.
    FileStatuses,
    /// The changes panel's diff index, polled every `GIT_POLL`.
    Index,
    /// `useProjectBranchesState`.
    Branches,
    /// `useProjectDiffStats`.
    DiffStats,
    /// `useProjectWorktrees`.
    Worktrees,
}

impl WatchKind {
    fn slot(self) -> usize {
        match self {
            WatchKind::FileStatuses => 0,
            WatchKind::Index => 1,
            WatchKind::Branches => 2,
            WatchKind::DiffStats => 3,
            WatchKind::Worktrees => 4,
        }
    }
}

type Watchers = Rc<RefCell<[usize; 5]>>;

/// Keeps one part of a folder's git status loading. Drop it to stop.
pub struct GitWatch {
    watchers: Watchers,
    kind: WatchKind,
}

impl Drop for GitWatch {
    fn drop(&mut self) {
        let mut counts = self.watchers.borrow_mut();
        let slot = &mut counts[self.kind.slot()];
        *slot = slot.saturating_sub(1);
    }
}

/// One project folder's git status.
pub struct GitStatus {
    cwd: String,
    backend: Arc<dyn ProjectsBackend>,
    clock: Clock,
    hidden: bool,
    watchers: Watchers,

    index: Option<GitDiffIndex>,
    statuses: GitStatusMap,
    index_in_flight: bool,
    index_pending: bool,
    index_reloads: u64,
    index_task: Option<Task<()>>,
    poll: Option<Task<()>>,
    /// This folder announced a git change from an index it just read, so the
    /// `git_changed` that comes back skips reading the index again.
    index_announced: bool,
    /// `refs_fingerprint_on_disk` as of the last index read.
    refs_fingerprint: Option<u64>,
    /// The folder with symlinks resolved, once `GitStatuses` looked it up.
    real_cwd: Option<String>,

    branches: ProjectBranchesState,
    branches_in_flight: bool,
    branches_task: Option<Task<()>>,

    stats: Option<GitDiffStats>,
    stats_in_flight: bool,
    stats_pending: bool,
    stats_epoch: u64,
    stats_loaded_at: i64,
    stats_task: Option<Task<()>>,

    worktrees: WorktreesSnapshot,
    worktrees_load: Option<Shared<Task<bool>>>,
    worktrees_invalidated: bool,
}

impl GitStatus {
    pub fn new(cwd: &str, backend: Arc<dyn ProjectsBackend>, clock: Clock, hidden: bool) -> Self {
        Self {
            cwd: cwd.to_string(),
            backend,
            clock,
            hidden,
            watchers: Rc::new(RefCell::new([0; 5])),
            index: None,
            statuses: GitStatusMap::default(),
            index_in_flight: false,
            index_pending: false,
            index_reloads: 0,
            index_task: None,
            poll: None,
            index_announced: false,
            refs_fingerprint: None,
            real_cwd: None,
            branches: ProjectBranchesState::default(),
            branches_in_flight: false,
            branches_task: None,
            stats: None,
            stats_in_flight: false,
            stats_pending: false,
            stats_epoch: 0,
            stats_loaded_at: 0,
            stats_task: None,
            worktrees: WorktreesSnapshot::default(),
            worktrees_load: None,
            worktrees_invalidated: false,
        }
    }

    // Reading.

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// The changed files and their folders (`useGitFileStatuses`).
    pub fn file_statuses(&self) -> &GitStatusMap {
        &self.statuses
    }

    /// The last diff index: branch, head, upstream, ahead and behind, and
    /// changed files with line counts.
    pub fn index(&self) -> Option<&GitDiffIndex> {
        self.index.as_ref()
    }

    /// `useProjectBranchesState`.
    pub fn branches_state(&self) -> &ProjectBranchesState {
        &self.branches
    }

    /// `useProjectBranches`.
    pub fn branches(&self) -> Option<&GitBranches> {
        self.branches.branches.as_ref()
    }

    /// `useProjectDiffStats`.
    pub fn diff_stats(&self) -> Option<&GitDiffStats> {
        self.stats.as_ref()
    }

    /// `useProjectWorktrees`.
    pub fn worktrees(&self) -> &WorktreesSnapshot {
        &self.worktrees
    }

    /// How many watchers a part has.
    pub fn watcher_count(&self, kind: WatchKind) -> usize {
        self.watchers.borrow()[kind.slot()]
    }

    fn watched(&self, kind: WatchKind) -> bool {
        self.watcher_count(kind) > 0
    }

    // Watching.

    /// Keep `kind` loading until the returned guard drops. The first watcher
    /// starts it, as the hooks' first subscriber did.
    pub fn watch(&mut self, kind: WatchKind, cx: &mut Context<Self>) -> GitWatch {
        let first = {
            let mut counts = self.watchers.borrow_mut();
            counts[kind.slot()] += 1;
            counts[kind.slot()] == 1
        };
        if first {
            self.start(kind, cx);
        }
        GitWatch {
            watchers: self.watchers.clone(),
            kind,
        }
    }

    fn start(&mut self, kind: WatchKind, cx: &mut Context<Self>) {
        match kind {
            WatchKind::FileStatuses => self.load_index(true, cx),
            WatchKind::Index => {
                let force = self.index_reloads > 0;
                self.load_index(force, cx);
                self.start_poll(cx);
            }
            WatchKind::Branches => self.load_branches(true, cx),
            WatchKind::DiffStats => {
                if !self.stats_in_flight && self.stats_stale() {
                    self.load_stats(true, cx);
                }
            }
            WatchKind::Worktrees => {
                drop(self.load_worktrees(false, cx));
            }
        }
    }

    fn start_poll(&mut self, cx: &mut Context<Self>) {
        if self.poll.is_some() {
            return;
        }
        self.poll = Some(cx.spawn(async move |this, cx| {
            let mut unfocused_ticks = 0;
            loop {
                let timer = cx.background_executor().timer(GIT_POLL);
                timer.await;
                let Some(this) = this.upgrade() else {
                    break;
                };
                let keep = cx.update(|cx| {
                    // No windows at all is the headless host, which polls
                    // as before.
                    let unfocused = cx.active_window().is_none() && !cx.windows().is_empty();
                    this.update(cx, |this, cx| {
                        if !this.watched(WatchKind::Index) {
                            this.poll = None;
                            return false;
                        }
                        if this.hidden {
                            return true;
                        }
                        if unfocused {
                            unfocused_ticks += 1;
                            if unfocused_ticks < GIT_POLL_UNFOCUSED_TICKS {
                                return true;
                            }
                        }
                        unfocused_ticks = 0;
                        this.load_index(false, cx);
                        true
                    })
                });
                if !keep {
                    break;
                }
            }
        }));
    }

    // Triggers.

    /// Window focus, or the window became visible (`onResume`).
    pub fn resume(&mut self, cx: &mut Context<Self>) {
        if self.hidden {
            return;
        }
        if self.watched(WatchKind::FileStatuses) {
            self.load_index(true, cx);
        } else if self.watched(WatchKind::Index) {
            self.load_index(false, cx);
        }
        if self.watched(WatchKind::Branches) {
            self.load_branches(true, cx);
        }
        if self.watched(WatchKind::DiffStats) && !self.stats_in_flight && self.stats_stale() {
            self.load_stats(true, cx);
        }
        if self.watched(WatchKind::Worktrees) {
            drop(self.load_worktrees(false, cx));
        }
    }

    /// `notifyGitChanged` reached this folder.
    pub fn git_changed(&mut self, cx: &mut Context<Self>) {
        let announced = std::mem::take(&mut self.index_announced);
        if !self.hidden {
            // When this folder's own new index caused the change, reading the
            // index again finds nothing new and costs another dozen git
            // processes.
            if !announced {
                if self.watched(WatchKind::FileStatuses) {
                    self.load_index(true, cx);
                } else if self.watched(WatchKind::Index) {
                    self.load_index(false, cx);
                }
            }
            if self.watched(WatchKind::Branches) {
                self.load_branches(true, cx);
            }
            if self.watched(WatchKind::Worktrees) {
                drop(self.load_worktrees(true, cx));
            }
        }
        // Diff stats reload on a git change even while hidden.
        if self.watched(WatchKind::DiffStats) {
            self.load_stats(true, cx);
        }
    }

    /// Another folder's poll (or this one's) saw edited files and no moved
    /// ref. This reloads what those files can change here and nothing else:
    /// the index and diff stats when this folder contains that folder or
    /// sits inside it, and the working copy list when a working copy may
    /// have turned clean or dirty. Branches stay as they are, because no ref
    /// moved. Relatedness goes by folder rather than by changed file: a
    /// rename lists only its new path, and the old one can sit in another
    /// watched folder.
    pub fn files_changed(&mut self, change: &FilesChange, cx: &mut Context<Self>) {
        // Only the entity that read the index already shows it. Another
        // spelling of the same folder is its own entity and reloads.
        let source = self.cwd == change.source;
        let related = source || self.related_to(change);
        if !self.hidden {
            if related && !source {
                if self.watched(WatchKind::FileStatuses) {
                    self.load_index(true, cx);
                } else if self.watched(WatchKind::Index) {
                    self.load_index(false, cx);
                }
            }
            if self.watched(WatchKind::Worktrees)
                && change.emptiness_changed
                && self.may_list_working_copy_of(change)
            {
                drop(self.load_worktrees(true, cx));
            }
        }
        // Diff stats reload on a git change even while hidden.
        if related && self.watched(WatchKind::DiffStats) {
            self.load_stats(true, cx);
        }
    }

    /// This folder holds the changed folder or sits inside it, under any
    /// spelling of either.
    fn related_to(&self, change: &FilesChange) -> bool {
        let mine = spellings(&self.cwd, self.real_cwd.as_deref());
        let theirs = spellings(&change.source, change.source_real.as_deref());
        mine.iter().any(|mine| {
            theirs
                .iter()
                .any(|theirs| within(mine, theirs) || within(theirs, mine))
        })
    }

    /// Whether this folder's working copy list can show the working copy
    /// holding the changed folder. An unloaded list counts as yes.
    fn may_list_working_copy_of(&self, change: &FilesChange) -> bool {
        let Some(data) = &self.worktrees.data else {
            return true;
        };
        if self.related_to(change) {
            return true;
        }
        let theirs = spellings(&change.source, change.source_real.as_deref());
        data.worktrees.iter().any(|tree| {
            let tree = comparable(&tree.path);
            theirs.iter().any(|theirs| within(theirs, &tree))
        })
    }

    /// `notifyDirsChanged` reached this folder.
    pub fn dirs_changed(&mut self, cx: &mut Context<Self>) {
        if !self.hidden && self.watched(WatchKind::FileStatuses) {
            self.load_index(true, cx);
        }
    }

    /// `document.hidden` changed.
    pub fn set_hidden(&mut self, hidden: bool, cx: &mut Context<Self>) {
        if self.hidden == hidden {
            return;
        }
        self.hidden = hidden;
        if !hidden {
            self.resume(cx);
        }
    }

    // The diff index and file statuses.

    /// `reload` in the changes panel: load now, even while hidden.
    pub fn reload_index(&mut self, cx: &mut Context<Self>) {
        self.index_reloads += 1;
        self.load_index(true, cx);
    }

    fn load_index(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.index_in_flight {
            self.index_pending = true;
            return;
        }
        if !force && self.hidden {
            return;
        }
        self.index_in_flight = true;
        let backend = self.backend.clone();
        let cwd = self.cwd.clone();
        // The poll runs every `GIT_POLL` while the changes panel shows, so
        // the status maps are built with the read, off the UI thread.
        let read = cx.background_spawn(async move {
            let index = backend.git_diff_index(&cwd).map(|index| {
                let statuses = build_status_maps(&index, &cwd);
                (index, statuses)
            });
            // After the index: a ref that moves while git runs then shows
            // on this read or the next one.
            let refs = backend.git_refs_fingerprint(&cwd);
            index.map(|(index, statuses)| (index, statuses, refs))
        });
        self.index_task = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            let Some(this) = this.upgrade() else {
                return;
            };
            cx.update(|cx| {
                this.update(cx, |this, cx| this.index_loaded(result, cx));
            });
        }));
    }

    fn index_loaded(
        &mut self,
        result: Result<(GitDiffIndex, GitStatusMap, Option<u64>), String>,
        cx: &mut Context<Self>,
    ) {
        let mut changed = false;
        match result {
            Ok((next, statuses, refs)) => {
                let prev_refs = std::mem::replace(&mut self.refs_fingerprint, refs);
                // The index carries no branch list or working copy list, so
                // a ref or working copy added without moving HEAD shows only
                // in the ref stamps. When they are missing or moved, every
                // folder reloads as before.
                let refs_held = prev_refs.is_some() && prev_refs == refs;
                if statuses != self.statuses {
                    self.statuses = statuses;
                    changed = true;
                }
                if self.watched(WatchKind::Index) && !same_index(self.index.as_ref(), &next) {
                    let prev = self.index.replace(next.clone());
                    changed = true;
                    self.apply_diff_stats(
                        GitDiffStats {
                            files: next.files.len() as i64,
                            additions: next.additions,
                            deletions: next.deletions,
                        },
                        cx,
                    );
                    if let Some(prev) = prev {
                        let paths = changed_file_paths(&prev, &next);
                        // A moved ref (commit, checkout, push, fetch) can
                        // change every folder of the repository, so it reloads
                        // everything. Edited files only reach the folders
                        // holding them, so only those reload.
                        let files = (refs_held && same_refs(&prev, &next)).then(|| FilesChange {
                            source: self.cwd.clone(),
                            source_real: self.real_cwd.clone(),
                            emptiness_changed: prev.files.is_empty() != next.files.is_empty(),
                        });
                        if files.is_none() {
                            self.index_announced = true;
                        }
                        cx.defer(move |cx| {
                            Engine::hooks(cx)
                                .workspace
                                .invalidate_watched_files(Some(&paths), cx);
                            match files {
                                Some(change) => super::notify_files_changed(&change, cx),
                                None => super::notify_git_changed(cx),
                            }
                        });
                    }
                }
            }
            Err(_) => {
                if self.statuses != GitStatusMap::default() {
                    self.statuses = GitStatusMap::default();
                    changed = true;
                }
                if self.watched(WatchKind::Index) && self.index.is_some() {
                    self.index = None;
                    changed = true;
                }
            }
        }
        self.index_in_flight = false;
        if changed {
            cx.notify();
        }
        if self.index_pending {
            self.index_pending = false;
            self.load_index(true, cx);
        }
    }

    // Branches.

    /// `seedProjectBranches`: show a list another window already loaded.
    pub fn seed_branches(&mut self, branches: GitBranches, cx: &mut Context<Self>) {
        self.publish_branches(Some(branches), cx);
    }

    fn publish_branches(&mut self, branches: Option<GitBranches>, cx: &mut Context<Self>) {
        // `settled` still flips on a lookup that found nothing, so an
        // unchanged `None` is only a no-op once the first one has landed.
        if self.branches.settled && self.branches.branches == branches {
            return;
        }
        self.branches = ProjectBranchesState {
            branches,
            settled: true,
        };
        cx.notify();
    }

    fn load_branches(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.branches_in_flight || (!force && self.hidden) {
            return;
        }
        self.branches_in_flight = true;
        let backend = self.backend.clone();
        let cwd = self.cwd.clone();
        let read = cx.background_spawn(async move { backend.git_branches(&cwd) });
        self.branches_task = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            let Some(this) = this.upgrade() else {
                return;
            };
            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    this.branches_in_flight = false;
                    this.publish_branches(result.ok(), cx);
                });
            });
        }));
    }

    // Diff stats.

    fn stats_stale(&self) -> bool {
        (self.clock)() - self.stats_loaded_at >= DIFF_STATS_RESUME_TTL_MS
    }

    fn publish_stats(&mut self, stats: Option<GitDiffStats>, cx: &mut Context<Self>) {
        if self.stats == stats {
            return;
        }
        self.stats = stats;
        cx.notify();
    }

    /// `applyProjectDiffStats`: stats from a fuller index, so the title bar
    /// badge cannot lag behind. A load already running is discarded.
    pub fn apply_diff_stats(&mut self, stats: GitDiffStats, cx: &mut Context<Self>) {
        self.stats_epoch += 1;
        self.stats_loaded_at = (self.clock)();
        self.publish_stats(Some(stats), cx);
    }

    fn load_stats(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.stats_in_flight {
            self.stats_pending = true;
            return;
        }
        if !force && self.hidden {
            return;
        }
        self.stats_in_flight = true;
        let epoch = self.stats_epoch;
        let backend = self.backend.clone();
        let cwd = self.cwd.clone();
        let read = cx.background_spawn(async move { backend.git_diff_stats(&cwd) });
        self.stats_task = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            let Some(this) = this.upgrade() else {
                return;
            };
            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    if epoch == this.stats_epoch {
                        this.stats_loaded_at = (this.clock)();
                        this.publish_stats(result.ok(), cx);
                    }
                    this.stats_in_flight = false;
                    if this.stats_pending {
                        this.stats_pending = false;
                        this.load_stats(true, cx);
                    }
                });
            });
        }));
    }

    // Worktrees.

    /// `refresh` from `useProjectWorktrees`: reload, with a follow-up if a
    /// read is already running. Resolves to whether the list loaded.
    pub fn refresh_worktrees(&mut self, cx: &mut Context<Self>) -> Shared<Task<bool>> {
        self.load_worktrees(true, cx)
    }

    fn load_worktrees(&mut self, invalidated: bool, cx: &mut Context<Self>) -> Shared<Task<bool>> {
        if let Some(in_flight) = &self.worktrees_load {
            // A git change during a read needs a follow-up; focus events can
            // share the read already running.
            self.worktrees_invalidated |= invalidated;
            return in_flight.clone();
        }
        let backend = self.backend.clone();
        let cwd = self.cwd.clone();
        let read = cx.background_spawn(async move { backend.git_worktrees(&cwd) });
        let load = cx
            .spawn(async move |this, cx| {
                let result = read.await;
                let Some(this) = this.upgrade() else {
                    return false;
                };
                let next = cx.update(|cx| {
                    this.update(cx, |this, cx| {
                        let loaded = match result {
                            Ok(data) => {
                                this.worktrees = WorktreesSnapshot {
                                    data: Some(data),
                                    error: None,
                                };
                                true
                            }
                            Err(error) => {
                                // A background failure keeps the usable list;
                                // the error shows beside it.
                                this.worktrees.error = Some(error);
                                false
                            }
                        };
                        cx.notify();
                        this.worktrees_load = None;
                        if this.worktrees_invalidated {
                            this.worktrees_invalidated = false;
                            return Err(this.load_worktrees(false, cx));
                        }
                        Ok(loaded)
                    })
                });
                match next {
                    Ok(loaded) => loaded,
                    Err(follow_up) => follow_up.await,
                }
            })
            .shared();
        self.worktrees_load = Some(load.clone());
        load
    }
}

/// Every folder's `GitStatus`, and the triggers that reach all of them.
pub struct GitStatuses {
    backend: Arc<dyn ProjectsBackend>,
    clock: Clock,
    hidden: bool,
    entries: HashMap<String, Entity<GitStatus>>,
    git_changed_scheduled: bool,
}

impl GitStatuses {
    pub fn new(backend: Arc<dyn ProjectsBackend>, clock: Clock) -> Self {
        Self {
            backend,
            clock,
            hidden: false,
            entries: HashMap::new(),
            git_changed_scheduled: false,
        }
    }

    /// `entryFor`: the folder's entity, created on first use.
    pub fn status(&mut self, cwd: &str, cx: &mut Context<Self>) -> Entity<GitStatus> {
        if let Some(entry) = self.entries.get(cwd) {
            return entry.clone();
        }
        let (backend, clock, hidden) = (self.backend.clone(), self.clock.clone(), self.hidden);
        let entry = cx.new(|_| GitStatus::new(cwd, backend.clone(), clock, hidden));
        self.entries.insert(cwd.to_string(), entry.clone());
        // Resolve symlinks once, off the UI thread, so a change seen under
        // one spelling of a folder reaches its other spellings.
        let (folder, path) = (entry.downgrade(), cwd.to_string());
        let real = cx.background_spawn(async move { backend.real_path(&path) });
        cx.spawn(async move |_, cx| {
            let real = real.await;
            cx.update(|cx| {
                if let Some(folder) = folder.upgrade() {
                    folder.update(cx, |status, _| status.real_cwd = real);
                }
            });
        })
        .detach();
        entry
    }

    /// The folder's entity, if anything asked for it yet.
    pub fn get(&self, cwd: &str) -> Option<Entity<GitStatus>> {
        self.entries.get(cwd).cloned()
    }

    fn each(&self, cx: &mut App, mut f: impl FnMut(&mut GitStatus, &mut Context<GitStatus>)) {
        let entries: Vec<Entity<GitStatus>> = self.entries.values().cloned().collect();
        for entry in entries {
            entry.update(cx, |status, cx| f(status, cx));
        }
    }

    /// `notifyGitChanged`. Calls within one effect cycle reload once.
    pub fn git_changed(&mut self, cx: &mut Context<Self>) {
        if self.git_changed_scheduled {
            return;
        }
        self.git_changed_scheduled = true;
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            let Some(this) = this.upgrade() else {
                return;
            };
            let entries = this.update(cx, |this, _| {
                this.git_changed_scheduled = false;
                this.entries.values().cloned().collect::<Vec<_>>()
            });
            for entry in entries {
                entry.update(cx, |status, cx| status.git_changed(cx));
            }
        });
    }

    /// `notifyDirsChanged`.
    pub fn dirs_changed(&mut self, cx: &mut Context<Self>) {
        self.each(cx, |status, cx| status.dirs_changed(cx));
    }

    /// Edited files with no moved ref: each folder reloads only what those
    /// files can change there (`GitStatus::files_changed`).
    pub fn files_changed(&mut self, change: &FilesChange, cx: &mut Context<Self>) {
        self.each(cx, |status, cx| status.files_changed(change, cx));
    }

    /// A window took focus.
    pub fn window_focused(&mut self, cx: &mut Context<Self>) {
        self.each(cx, |status, cx| status.resume(cx));
    }

    /// `document.hidden` changed for the app.
    pub fn set_hidden(&mut self, hidden: bool, cx: &mut Context<Self>) {
        if self.hidden == hidden {
            return;
        }
        self.hidden = hidden;
        self.each(cx, |status, cx| status.set_hidden(hidden, cx));
    }

    pub fn hidden(&self) -> bool {
        self.hidden
    }

    /// `applyProjectDiffStats` for a folder, from a fuller index elsewhere.
    pub fn apply_diff_stats(&mut self, cwd: &str, stats: GitDiffStats, cx: &mut Context<Self>) {
        if cwd.is_empty() || cwd == "~" {
            return;
        }
        let entry = self.status(cwd, cx);
        entry.update(cx, |status, cx| status.apply_diff_stats(stats, cx));
    }

    /// `seedProjectBranches`.
    pub fn seed_branches(&mut self, cwd: &str, branches: GitBranches, cx: &mut Context<Self>) {
        if cwd.is_empty() || cwd == "~" {
            return;
        }
        let entry = self.status(cwd, cx);
        entry.update(cx, |status, cx| status.seed_branches(branches, cx));
    }
}

/// Whether a folder can have git status at all (`enabled && cwd && cwd !==
/// "~"`).
pub fn git_status_enabled(cwd: &str) -> bool {
    !cwd.is_empty() && cwd != "~"
}

#[cfg(test)]
mod tests {
    use super::*;
    use monocode_git::fs::GitChangedFile;

    #[test]
    fn the_refs_stamp_moves_with_new_branches_and_working_copies() {
        let root = std::env::temp_dir().join(format!("monocode-refs-{}", uuid::Uuid::new_v4()));
        let git = root.join("main/.git");
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git.join("refs/heads/main"), "abc\n").unwrap();
        std::fs::create_dir_all(root.join("main/src")).unwrap();
        let cwd = root.join("main/src").to_string_lossy().into_owned();
        let first = refs_fingerprint_on_disk(&cwd);
        assert!(first.is_some());
        assert_eq!(refs_fingerprint_on_disk(&cwd), first);
        // `git branch feature/x`.
        std::fs::create_dir_all(git.join("refs/heads/feature")).unwrap();
        std::fs::write(git.join("refs/heads/feature/x"), "abc\n").unwrap();
        let branched = refs_fingerprint_on_disk(&cwd);
        assert_ne!(branched, first);
        // `git worktree add`, seen from the linked working copy too.
        let linked = git.join("worktrees/linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        std::fs::write(linked.join("commondir"), "../..\n").unwrap();
        std::fs::create_dir_all(root.join("linked")).unwrap();
        std::fs::write(
            root.join("linked/.git"),
            format!("gitdir: {}\n", linked.display()),
        )
        .unwrap();
        let added = refs_fingerprint_on_disk(&cwd);
        assert_ne!(added, branched);
        let from_linked = refs_fingerprint_on_disk(&root.join("linked").to_string_lossy());
        assert!(from_linked.is_some());
        // Outside any repository there is nothing to stamp.
        assert_eq!(refs_fingerprint_on_disk(&root.to_string_lossy()), None);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn folders_compare_without_trailing_slashes() {
        assert!(within(&comparable("/repo/sub/"), &comparable("/repo/")));
        assert!(within(&comparable("/repo"), &comparable("/repo/")));
        assert!(!within(&comparable("/repository"), &comparable("/repo")));
        assert!(within(&comparable("/anything"), &comparable("/")));
    }

    fn changed(path: &str, relative: &str, status: &str) -> GitChangedFile {
        GitChangedFile {
            path: path.into(),
            relative: relative.into(),
            status: status.into(),
            additions: 1,
            deletions: 0,
            staged: false,
            unstaged: true,
        }
    }

    #[test]
    fn folders_take_their_strongest_child_status() {
        let index = GitDiffIndex {
            files: vec![
                changed("/repo/src/a/new.ts", "src/a/new.ts", "untracked"),
                changed("/repo/src/a/old.ts", "src/a/old.ts", "modified"),
                changed("/repo/src/b.ts", "src/b.ts", "deleted"),
                changed("/repo/top.ts", "top.ts", "renamed"),
            ],
            ..GitDiffIndex::default()
        };
        let map = build_status_maps(&index, "/repo");
        assert_eq!(map.files.len(), 4);
        assert_eq!(
            map.dirs.get("/repo/src/a").map(String::as_str),
            Some("modified")
        );
        assert_eq!(
            map.dirs.get("/repo/src").map(String::as_str),
            Some("modified")
        );
        assert!(!map.dirs.contains_key("/repo"));
    }

    #[test]
    fn parent_paths_follow_the_typescript_helper() {
        assert_eq!(parent_path("/repo/src/a.ts"), "/repo/src");
        assert_eq!(parent_path("/repo"), "/");
        assert_eq!(parent_path("C:/repo"), "C:/");
        assert_eq!(parent_path("C:"), "C:/");
        assert_eq!(parent_path("//server/share"), "//server/share");
        assert_eq!(parent_path("//server/share/x"), "//server/share");
    }

    #[test]
    fn same_index_ignores_absolute_paths_only() {
        let prev = GitDiffIndex {
            files: vec![changed("/a/x.ts", "x.ts", "modified")],
            ..GitDiffIndex::default()
        };
        let moved = GitDiffIndex {
            files: vec![changed("/b/x.ts", "x.ts", "modified")],
            ..GitDiffIndex::default()
        };
        assert!(same_index(Some(&prev), &moved));
        assert!(!same_index(None, &moved));
        let ahead = GitDiffIndex {
            ahead: 1,
            ..moved.clone()
        };
        assert!(!same_index(Some(&prev), &ahead));
    }

    #[test]
    fn changed_paths_cover_new_changed_and_gone_files() {
        let prev = GitDiffIndex {
            files: vec![
                changed("/r/keep.ts", "keep.ts", "modified"),
                changed("/r/gone.ts", "gone.ts", "modified"),
                changed("/r/edit.ts", "edit.ts", "modified"),
            ],
            ..GitDiffIndex::default()
        };
        let mut edit = changed("/r/edit.ts", "edit.ts", "modified");
        edit.additions = 9;
        let next = GitDiffIndex {
            files: vec![
                changed("/r/keep.ts", "keep.ts", "modified"),
                edit,
                changed("/r/new.ts", "new.ts", "untracked"),
            ],
            ..GitDiffIndex::default()
        };
        assert_eq!(
            changed_file_paths(&prev, &next),
            ["/r/edit.ts", "/r/new.ts", "/r/gone.ts"]
        );
    }
}
