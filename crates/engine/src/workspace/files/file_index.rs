//! Port of src/features/files/model/fileIndex.ts: the project file index
//! behind Quick Open, `@` mentions, and link resolution, plus the
//! per-project list of recently opened files.
//!
//! The TypeScript kept one module-level cache. Here it is the `FileIndex`
//! entity, one per app, and observers replace `subscribeProjectFiles`
//! (`cx.observe(&index, ..)`). Views that only care about one project
//! subscribe to [`ProjectFilesChanged`] instead.
//!
//! The TypeScript cached the listing of one project at a time. Every open
//! session pane reads the listing of its own project, so with one slot two
//! panes in different projects evicted each other's listing and re-listed
//! forever. The index keeps one listing per project, up to
//! [`MAX_CACHED_PROJECTS`].

use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt;
use futures::future::Shared;
use gpui::{Context, EventEmitter, Task};

use super::backend::{FsBackend, ProjectFile};
use crate::runtime::util::fuzzy::score_path;
use crate::workspace::paths::{looks_like_project, normalize_editor_path, resolve_workspace_path};

const MAX_RECENTS: usize = 30;
const MAX_RESULTS: usize = 80;
/// The most project listings the index keeps. The least recently used one
/// goes first.
pub const MAX_CACHED_PROJECTS: usize = 8;
/// `REFRESH_MS`: the debounce before a directory change re-lists the project.
pub const REFRESH_DELAY: Duration = Duration::from_millis(150);
/// How long a failed scan answers for its project. Views reload when a scan
/// ends, so without this a project that cannot be listed (a removed
/// worktree, a folder without permission) is re-listed in a loop.
pub const FAILED_SCAN_RETRY: Duration = Duration::from_secs(10);

/// A finished listing. Errors are the backend's message.
pub type FilesResult = Result<Arc<Vec<ProjectFile>>, String>;

/// A listing in progress that any number of callers can await.
pub type FilesLoad = Shared<Task<FilesResult>>;

/// The listing of `cwd` changed. A scan that finds the same files does not
/// emit it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectFilesChanged {
    pub cwd: String,
}

/// How far a cached listing can be trusted. `peek_project_files` returns
/// it in every state, so views do not flash empty while it re-lists, and
/// `load_project_files` scans again in every state but `Fresh`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Freshness {
    Fresh,
    /// Files may have changed outside the app while its windows were away.
    /// The next read re-lists it ([`FileIndex::revalidate`]).
    Unknown,
    /// An edit or a directory change in the app outdated it. The next
    /// refresh re-lists it.
    Invalidated,
}

struct Cache {
    files: Arc<Vec<ProjectFile>>,
    freshness: Freshness,
    /// The index clock when the listing was last read, for eviction.
    used: Cell<u64>,
}

struct Inflight {
    id: u64,
    load: FilesLoad,
}

/// `normCwd`.
fn norm_cwd(cwd: &str) -> String {
    let slashed = monocode_core::paths::slash(cwd);
    let trimmed = slashed.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".into()
    } else {
        trimmed.to_string()
    }
}

/// The project file index.
pub struct FileIndex {
    backend: Arc<dyn FsBackend>,
    caches: HashMap<String, Cache>,
    inflight: HashMap<String, Inflight>,
    /// The last failed scan of each project, and when it failed.
    failed: HashMap<String, (Instant, String)>,
    last_cwd: Option<String>,
    epoch: u64,
    clock: Cell<u64>,
    refresh_timer: Option<Task<()>>,
    refreshing: bool,
    refresh_again: bool,
    recents_by_cwd: HashMap<String, Vec<String>>,
    /// `document.hidden`: every window is hidden or minimized.
    hidden: bool,
}

impl EventEmitter<ProjectFilesChanged> for FileIndex {}

impl FileIndex {
    pub fn new(backend: Arc<dyn FsBackend>) -> Self {
        Self {
            backend,
            caches: HashMap::new(),
            inflight: HashMap::new(),
            failed: HashMap::new(),
            last_cwd: None,
            epoch: 0,
            clock: Cell::new(0),
            refresh_timer: None,
            refreshing: false,
            refresh_again: false,
            recents_by_cwd: HashMap::new(),
            hidden: false,
        }
    }

    fn touch(&self, cache: &Cache) {
        let now = self.clock.get() + 1;
        self.clock.set(now);
        cache.used.set(now);
    }

    /// `peekProjectFiles`: the cached listing for `cwd`, if there is one.
    /// An outdated listing is returned until the new scan lands.
    pub fn peek_project_files(&self, cwd: &str) -> Option<Arc<Vec<ProjectFile>>> {
        let cache = self.caches.get(cwd)?;
        self.touch(cache);
        Some(cache.files.clone())
    }

    /// `invalidateProjectFiles`: the listing for `cwd` is out of date, and
    /// the next refresh re-lists it. `None` marks every listing as possibly
    /// outdated, so each re-lists when it is next read. The old listing
    /// stays readable until the re-list lands, and a re-list that finds the
    /// same files notifies nobody.
    pub fn invalidate_project_files(&mut self, cwd: Option<&str>, cx: &mut Context<Self>) {
        match cwd {
            Some(cwd) => {
                self.failed.remove(cwd);
                let cached = self.caches.get_mut(cwd);
                let running = self.inflight.remove(cwd).is_some();
                if cached.is_none() && !running {
                    return;
                }
                if let Some(cache) = cached {
                    cache.freshness = Freshness::Invalidated;
                }
            }
            None => {
                self.inflight.clear();
                self.failed.clear();
                self.mark_unknown();
            }
        }
        self.schedule_index_refresh(cx);
    }

    /// Every fresh listing may be out of date.
    fn mark_unknown(&mut self) {
        for cache in self.caches.values_mut() {
            if cache.freshness == Freshness::Fresh {
                cache.freshness = Freshness::Unknown;
            }
        }
    }

    fn has_refresh_targets(&self) -> bool {
        self.last_cwd.is_some()
            || self
                .caches
                .values()
                .any(|cache| cache.freshness == Freshness::Invalidated)
    }

    /// Re-list `cwd` in the background if its listing may be out of date
    /// and no scan is running. Views call this when they read the listing;
    /// a changed listing arrives as [`ProjectFilesChanged`].
    pub fn revalidate(&mut self, cwd: &str, cx: &mut Context<Self>) {
        let outdated = self
            .caches
            .get(cwd)
            .is_some_and(|cache| cache.freshness != Freshness::Fresh);
        if outdated && !self.hidden && !self.inflight.contains_key(cwd) {
            drop(self.start_scan(cwd, cx));
        }
    }

    /// `scheduleIndexRefresh`: re-list the last project, and every
    /// invalidated listing, after the debounce.
    pub fn schedule_index_refresh(&mut self, cx: &mut Context<Self>) {
        if !self.has_refresh_targets() || self.hidden || self.refresh_timer.is_some() {
            return;
        }
        let timer = cx.background_executor().timer(REFRESH_DELAY);
        self.refresh_timer = Some(cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |this, cx| {
                this.refresh_timer = None;
                this.run_index_refresh(cx);
            })
            .ok();
        }));
    }

    /// `runIndexRefresh`.
    fn run_index_refresh(&mut self, cx: &mut Context<Self>) {
        if self.refreshing {
            self.refresh_again = true;
            return;
        }
        let mut targets: Vec<String> = self.last_cwd.iter().cloned().collect();
        for (cwd, cache) in &self.caches {
            if cache.freshness == Freshness::Invalidated && !targets.contains(cwd) {
                targets.push(cwd.clone());
            }
        }
        if targets.is_empty() {
            return;
        }
        self.refreshing = true;
        let loads: Vec<FilesLoad> = targets.iter().map(|cwd| self.start_scan(cwd, cx)).collect();
        cx.spawn(async move |this, cx| {
            // The next focus or directory change retries a failed scan.
            futures::future::join_all(loads).await;
            this.update(cx, |this, cx| {
                this.refreshing = false;
                if this.refresh_again {
                    this.refresh_again = false;
                    this.schedule_index_refresh(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// A window became visible or took focus: the TypeScript `focus` and
    /// `visibilitychange` listeners. Files may have changed outside the app.
    /// The last project re-lists now, as in the TypeScript; the others
    /// re-list when they are next read.
    pub fn window_shown(&mut self, cx: &mut Context<Self>) {
        if !self.hidden {
            self.mark_unknown();
            self.schedule_index_refresh(cx);
        }
    }

    /// `document.hidden` changed.
    pub fn set_hidden(&mut self, hidden: bool, cx: &mut Context<Self>) {
        self.hidden = hidden;
        if !hidden {
            self.mark_unknown();
            self.schedule_index_refresh(cx);
        }
    }

    /// `rememberOpenedFile`.
    pub fn remember_opened_file(&mut self, cwd: &str, path: &str) {
        if path.is_empty() {
            return;
        }
        let recents = self.recents_by_cwd.entry(norm_cwd(cwd)).or_default();
        recents.retain(|item| item != path);
        recents.insert(0, path.to_string());
        recents.truncate(MAX_RECENTS);
    }

    /// `recentOpenedFiles`.
    pub fn recent_opened_files(&self, cwd: &str) -> Vec<String> {
        self.recents_by_cwd
            .get(&norm_cwd(cwd))
            .cloned()
            .unwrap_or_default()
    }

    /// `prefetchProjectFiles`.
    pub fn prefetch_project_files(&mut self, cwd: &str, cx: &mut Context<Self>) {
        if !looks_like_project(cwd) {
            return;
        }
        // The index keeps the scan running; nothing waits for it here.
        drop(self.load_project_files(cwd, false, cx));
    }

    /// `loadProjectFiles`: the cached listing, the scan already running, or
    /// a new scan. `refresh` always starts a new scan, and so does a
    /// listing that is not fresh.
    pub fn load_project_files(
        &mut self,
        cwd: &str,
        refresh: bool,
        cx: &mut Context<Self>,
    ) -> FilesLoad {
        if !looks_like_project(cwd) {
            return Task::ready(Ok(Arc::new(Vec::new()))).shared();
        }
        self.last_cwd = Some(cwd.to_string());
        if !refresh
            && let Some(cache) = self
                .caches
                .get(cwd)
                .filter(|cache| cache.freshness == Freshness::Fresh)
        {
            self.touch(cache);
            return Task::ready(Ok(cache.files.clone())).shared();
        }
        if !refresh && let Some(inflight) = self.inflight.get(cwd) {
            return inflight.load.clone();
        }
        if !refresh
            && let Some((at, error)) = self.failed.get(cwd)
            && cx.background_executor().now().duration_since(*at) < FAILED_SCAN_RETRY
        {
            return Task::ready(Err(error.clone())).shared();
        }
        self.start_scan(cwd, cx)
    }

    /// List `cwd` on the background executor. The result replaces the
    /// cached listing unless a newer scan or an invalidation came after it.
    fn start_scan(&mut self, cwd: &str, cx: &mut Context<Self>) -> FilesLoad {
        self.epoch += 1;
        let id = self.epoch;
        let previous = self.caches.get(cwd).map(|cache| cache.files.clone());
        let listing = self.backend.list_project_files(cwd.to_string());
        let scan = cx.background_executor().spawn(async move {
            let files = listing.await?;
            // The same files keep the old `Arc`, so readers can tell nothing
            // changed with a pointer compare.
            Ok(match previous {
                Some(previous) if *previous == files => previous,
                _ => Arc::new(files),
            })
        });
        let cwd_owned = cwd.to_string();
        let load = cx
            .spawn(async move |this, cx| {
                let result: FilesResult = scan.await;
                this.update(cx, |this, cx| {
                    if !this
                        .inflight
                        .get(&cwd_owned)
                        .is_some_and(|inflight| inflight.id == id)
                    {
                        return;
                    }
                    this.inflight.remove(&cwd_owned);
                    match &result {
                        Ok(files) => {
                            this.failed.remove(&cwd_owned);
                            this.store(cwd_owned, files.clone(), cx);
                        }
                        Err(error) => {
                            let now = cx.background_executor().now();
                            this.failed.insert(cwd_owned.clone(), (now, error.clone()));
                            // A listing that could not be re-listed may name
                            // files that are gone, such as a removed worktree.
                            if this
                                .caches
                                .get(&cwd_owned)
                                .is_some_and(|cache| cache.freshness != Freshness::Fresh)
                            {
                                this.caches.remove(&cwd_owned);
                                cx.emit(ProjectFilesChanged { cwd: cwd_owned });
                                cx.notify();
                            }
                        }
                    }
                })
                .ok();
                result
            })
            .shared();
        self.inflight.insert(
            cwd.to_string(),
            Inflight {
                id,
                load: load.clone(),
            },
        );
        load
    }

    fn store(&mut self, cwd: String, files: Arc<Vec<ProjectFile>>, cx: &mut Context<Self>) {
        let changed = match self.caches.get_mut(&cwd) {
            Some(cache) => {
                let changed = !Arc::ptr_eq(&cache.files, &files);
                cache.files = files;
                cache.freshness = Freshness::Fresh;
                changed
            }
            None => {
                self.caches.insert(
                    cwd.clone(),
                    Cache {
                        files,
                        freshness: Freshness::Fresh,
                        used: Cell::new(0),
                    },
                );
                true
            }
        };
        if let Some(cache) = self.caches.get(&cwd) {
            self.touch(cache);
        }
        while self.caches.len() > MAX_CACHED_PROJECTS {
            let Some(oldest) = self
                .caches
                .iter()
                .filter(|(key, _)| **key != cwd)
                .min_by_key(|(_, cache)| cache.used.get())
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.caches.remove(&oldest);
        }
        if changed {
            cx.emit(ProjectFilesChanged { cwd });
            cx.notify();
        }
    }

    /// `resolveOpenablePath`: resolve a transcript or markdown file link to
    /// an existing project file.
    pub fn resolve_openable_path(
        &mut self,
        cwd: &str,
        href: &str,
        cx: &mut Context<Self>,
    ) -> Task<Option<String>> {
        let Some(direct) = resolve_workspace_path(href, Some(cwd)) else {
            return Task::ready(None);
        };
        let load = self.load_project_files(cwd, false, cx);
        let cwd = cwd.to_string();
        let href = href.to_string();
        cx.spawn(async move |this, cx| {
            let files = match load.await {
                Ok(files) => files,
                // The index only disambiguates shortened paths. Let the
                // editor read the direct path and show its own error if that
                // file is unavailable too.
                Err(_) => return Some(direct),
            };
            let recents = this
                .read_with(cx, |this, _| this.recent_opened_files(&cwd))
                .unwrap_or_default();
            Some(pick_openable_path(&files, &cwd, &href, direct, &recents))
        })
    }

    /// `resolveFileOpenRequest`: resolve shortened references while keeping
    /// paths picked from file UI.
    pub fn resolve_file_open_request(
        &mut self,
        cwd: &str,
        path: &str,
        exact: bool,
        cx: &mut Context<Self>,
    ) -> Task<String> {
        if exact {
            return Task::ready(path.to_string());
        }
        let resolving = self.resolve_openable_path(cwd, path, cx);
        let path = path.to_string();
        cx.spawn(async move |_, _| resolving.await.unwrap_or(path))
    }
}

impl FileIndex {
    /// `applyFileMentionsToTurn` from fileMentions.ts: spell out where each
    /// `@name` in a prompt lives before it goes to the agent.
    pub fn apply_file_mentions_to_turn(
        &mut self,
        text: &str,
        cwd: &str,
        cx: &mut Context<Self>,
    ) -> Task<String> {
        if !super::file_mentions::looks_mentioned(text) {
            return Task::ready(text.to_string());
        }
        let load = self.load_project_files(cwd, false, cx);
        let text = text.to_string();
        cx.spawn(async move |_, _| {
            let files = load.await.unwrap_or_default();
            super::file_mentions::apply_file_mentions(&text, &files)
        })
    }
}

/// The part of `resolveOpenablePath` after the index loaded.
pub fn pick_openable_path(
    files: &[ProjectFile],
    cwd: &str,
    href: &str,
    direct: String,
    recents: &[String],
) -> String {
    if files.is_empty() {
        return direct;
    }
    let normalized_direct = normalize_editor_path(&direct);
    if let Some(exact) = files
        .iter()
        .find(|file| normalize_editor_path(&file.path) == normalized_direct)
    {
        return exact.path.clone();
    }

    let rel_hint = relative_path_hint(href, cwd, &direct);
    if let Some(exact_relative) = files
        .iter()
        .find(|file| file.relative == rel_hint || normalize_editor_path(&file.relative) == rel_hint)
    {
        return exact_relative.path.clone();
    }

    let suffix_matches: Vec<&ProjectFile> = files
        .iter()
        .filter(|file| {
            file.relative == rel_hint
                || file.relative.ends_with(&format!("/{rel_hint}"))
                || rel_hint.ends_with(&file.relative)
        })
        .collect();
    if suffix_matches.len() == 1 {
        return suffix_matches[0].path.clone();
    }

    let base_name = rel_hint
        .split('/')
        .rfind(|part| !part.is_empty())
        .unwrap_or(&rel_hint)
        .to_string();
    let by_name: Vec<&ProjectFile> = files.iter().filter(|file| file.name == base_name).collect();
    match by_name.len() {
        0 => direct,
        1 => by_name[0].path.clone(),
        _ => pick_openable_file(by_name, &rel_hint, recents).path.clone(),
    }
}

/// `relativePathHint`.
fn relative_path_hint(href: &str, cwd: &str, direct: &str) -> String {
    let mut value = monocode_core::js::trim(href).replace('\\', "/");
    value = strip_line_suffix(&value).to_string();
    if let Some(rest) = value.strip_prefix("file://") {
        value = crate::workspace::paths::decode_uri_component(rest)
            .unwrap_or_else(|| rest.to_string())
            .replace('\\', "/");
    }
    let value = value
        .strip_prefix("./")
        .unwrap_or(&value)
        .trim_start_matches('/')
        .to_string();

    let base = cwd.replace('\\', "/");
    let base = base.trim_end_matches('/');
    let normalized_direct = normalize_editor_path(direct);
    if !base.is_empty()
        && base != "~"
        && let Some(rest) = normalized_direct.strip_prefix(&format!("{base}/"))
    {
        return rest.to_string();
    }
    value
}

/// `/(?::\d+(?::\d+)?|#L\d+(?:-L\d+)?)$/` removed from the end.
fn strip_line_suffix(value: &str) -> &str {
    static SUFFIX: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?::\d+(?::\d+)?|#L\d+(?:-L\d+)?)$").expect("line suffix pattern")
    });
    match SUFFIX.find(value) {
        Some(found) => &value[..found.start()],
        None => value,
    }
}

/// `pickOpenableFile`: a recent file first, then the shortest suffix
/// match, then the shortest path.
fn pick_openable_file<'a>(
    mut candidates: Vec<&'a ProjectFile>,
    rel_hint: &str,
    recents: &[String],
) -> &'a ProjectFile {
    for recent in recents {
        let normalized_recent = normalize_editor_path(recent);
        if let Some(hit) = candidates
            .iter()
            .find(|file| normalize_editor_path(&file.path) == normalized_recent)
        {
            return hit;
        }
    }

    let mut suffix_matches: Vec<&ProjectFile> = candidates
        .iter()
        .copied()
        .filter(|file| {
            file.relative == rel_hint || file.relative.ends_with(&format!("/{rel_hint}"))
        })
        .collect();
    if !suffix_matches.is_empty() {
        suffix_matches.sort_by_key(|file| monocode_core::js::len(&file.relative));
        return suffix_matches[0];
    }
    candidates.sort_by_key(|file| monocode_core::js::len(&file.relative));
    candidates[0]
}

/// `RankedFile`: a file with its fuzzy score and the matched positions in
/// `relative`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedFile {
    pub file: ProjectFile,
    pub score: i64,
    pub positions: Vec<usize>,
}

/// `rankProjectFiles` with the default limit.
pub fn rank_project_files(
    files: &[ProjectFile],
    query: &str,
    recents: &[String],
) -> Vec<RankedFile> {
    rank_project_files_limit(files, query, recents, MAX_RESULTS)
}

/// `rankProjectFiles`: recents without a query, fuzzy matches with one.
pub fn rank_project_files_limit(
    files: &[ProjectFile],
    query: &str,
    recents: &[String],
    limit: usize,
) -> Vec<RankedFile> {
    let recent_rank: HashMap<&str, usize> = recents
        .iter()
        .enumerate()
        .map(|(index, path)| (path.as_str(), index))
        .collect();

    if monocode_core::js::trim(query).is_empty() {
        let by_path: HashMap<&str, &ProjectFile> = files
            .iter()
            .map(|file| (file.path.as_str(), file))
            .collect();
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for path in recents {
            if !seen.insert(path.as_str()) {
                continue;
            }
            let Some(file) = by_path.get(path.as_str()) else {
                continue;
            };
            out.push(RankedFile {
                file: (*file).clone(),
                score: 0,
                positions: Vec::new(),
            });
            if out.len() >= limit {
                break;
            }
        }
        return out;
    }

    // Score by listing position and clone only the rows that make the cut.
    // A short query matches most of a large project, and this runs on every
    // keystroke.
    let mut scored: Vec<(usize, i64, Vec<usize>)> = files
        .iter()
        .enumerate()
        .filter_map(|(at, file)| {
            let hit = score_path(query, &file.relative, &file.name)?;
            let recency = recent_rank
                .get(file.path.as_str())
                .map_or(0, |recency| (MAX_RECENTS as i64 - *recency as i64) * 8);
            Some((at, hit.score + recency, hit.positions))
        })
        .collect();
    let order = |a: &(usize, i64, Vec<usize>), b: &(usize, i64, Vec<usize>)| {
        let (left, right) = (&files[a.0], &files[b.0]);
        b.1.cmp(&a.1)
            .then_with(|| {
                monocode_core::js::len(&left.relative).cmp(&monocode_core::js::len(&right.relative))
            })
            .then_with(|| locale_compare(&left.relative, &right.relative))
            // The TypeScript sort was stable, so equal rows keep listing order.
            .then_with(|| a.0.cmp(&b.0))
    };
    if limit == 0 {
        return Vec::new();
    }
    if scored.len() > limit {
        scored.select_nth_unstable_by(limit - 1, order);
        scored.truncate(limit);
    }
    scored.sort_by(order);
    scored
        .into_iter()
        .map(|(at, score, positions)| RankedFile {
            file: files[at].clone(),
            score,
            positions,
        })
        .collect()
}

/// `String.prototype.localeCompare` for paths with the OS default locale.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    monocode_locale::compare(a, b)
}

#[cfg(test)]
mod tests {
    use super::super::backend::fake::FakeFs;
    use super::*;
    use gpui::{AppContext, Entity, TestAppContext};

    const CWD: &str = "/Users/me/project";

    fn assert_ranked_names(input: &[&str], expected: &[&str]) {
        for locale in ["en", "fr", "ja", "ar"] {
            monocode_locale::with_locale(locale, || {
                let files: Vec<_> = input
                    .iter()
                    .map(|name| ProjectFile::new(*name, format!("{CWD}/{name}"), *name))
                    .collect();
                let ranked = rank_project_files(&files, "file", &[]);
                assert_eq!(ranked.len(), input.len());
                assert!(ranked.windows(2).all(|pair| pair[0].score == pair[1].score));
                assert!(ranked.windows(2).all(|pair| monocode_core::js::len(
                    &pair[0].file.relative
                ) == monocode_core::js::len(
                    &pair[1].file.relative
                )));
                assert_eq!(
                    ranked
                        .iter()
                        .map(|hit| hit.file.relative.as_str())
                        .collect::<Vec<_>>(),
                    expected
                );
            })
            .unwrap();
        }
    }

    #[test]
    fn matches_intl_file_ranking_punctuation() {
        assert_ranked_names(
            &["file.a.rs", "file-a.rs", "file_a.rs"],
            &["file_a.rs", "file-a.rs", "file.a.rs"],
        );
    }

    #[test]
    fn matches_intl_file_ranking_accents() {
        assert_ranked_names(
            &["filez.rs", "fileé.rs", "filee.rs"],
            &["filee.rs", "fileé.rs", "filez.rs"],
        );
    }

    #[test]
    fn matches_intl_file_ranking_canonical_equivalence() {
        assert_ranked_names(&["fileÅ.rs", "fileÅ.rs"], &["fileÅ.rs", "fileÅ.rs"]);
    }

    fn files() -> Vec<ProjectFile> {
        vec![
            ProjectFile::new(
                "App.tsx",
                "/Users/me/project/apps/desktop/src/App.tsx",
                "apps/desktop/src/App.tsx",
            ),
            ProjectFile::new(
                "App.tsx",
                "/Users/me/project/apps/web/src/App.tsx",
                "apps/web/src/App.tsx",
            ),
            ProjectFile::new(
                "main.tsx",
                "/Users/me/project/apps/desktop/src/main.tsx",
                "apps/desktop/src/main.tsx",
            ),
        ]
    }

    fn extra() -> ProjectFile {
        ProjectFile::new("pasted.ts", "/Users/me/project/pasted.ts", "pasted.ts")
    }

    fn setup(cx: &mut TestAppContext) -> (Arc<FakeFs>, Entity<FileIndex>) {
        let fs = FakeFs::new();
        fs.set_files(files());
        let index = cx.new(|_| FileIndex::new(fs.clone()));
        (fs, index)
    }

    fn resolve(index: &Entity<FileIndex>, href: &str, cx: &mut TestAppContext) -> Option<String> {
        let task = index.update(cx, |index, cx| index.resolve_openable_path(CWD, href, cx));
        cx.run_until_parked();
        task.now_or_never().expect("resolved")
    }

    fn load(index: &Entity<FileIndex>, refresh: bool, cx: &mut TestAppContext) -> FilesLoad {
        index.update(cx, |index, cx| index.load_project_files(CWD, refresh, cx))
    }

    fn settle(load: FilesLoad, cx: &mut TestAppContext) -> Vec<ProjectFile> {
        cx.run_until_parked();
        load.now_or_never()
            .expect("loaded")
            .map(|files| files.to_vec())
            .unwrap_or_default()
    }

    fn settle_result(load: FilesLoad, cx: &mut TestAppContext) -> FilesResult {
        cx.run_until_parked();
        load.now_or_never().expect("loaded")
    }

    fn settle_arc(load: FilesLoad, cx: &mut TestAppContext) -> Arc<Vec<ProjectFile>> {
        cx.run_until_parked();
        load.now_or_never().expect("loaded").expect("listed")
    }

    #[gpui::test]
    fn maps_a_basename_only_link_to_the_shortest_matching_project_path(cx: &mut TestAppContext) {
        let (_, index) = setup(cx);
        assert_eq!(
            resolve(&index, "App.tsx", cx),
            Some(files()[1].path.clone())
        );
    }

    #[gpui::test]
    fn prefers_recently_opened_files_for_ambiguous_basenames(cx: &mut TestAppContext) {
        let (_, index) = setup(cx);
        index.update(cx, |index, _| {
            index.remember_opened_file(CWD, &files()[1].path)
        });
        assert_eq!(
            resolve(&index, "App.tsx", cx),
            Some(files()[1].path.clone())
        );
        index.update(cx, |index, _| {
            index.remember_opened_file(CWD, &files()[0].path)
        });
        assert_eq!(
            resolve(&index, "App.tsx", cx),
            Some(files()[0].path.clone())
        );
    }

    #[gpui::test]
    fn matches_a_relative_project_path(cx: &mut TestAppContext) {
        let (_, index) = setup(cx);
        assert_eq!(
            resolve(&index, "apps/desktop/src/main.tsx", cx),
            Some(files()[2].path.clone())
        );
    }

    #[gpui::test]
    fn still_opens_a_direct_file_when_the_optional_project_index_is_unavailable(
        cx: &mut TestAppContext,
    ) {
        let (fs, index) = setup(cx);
        fs.fail_files("Project scan unavailable");
        assert_eq!(
            resolve(&index, "apps/desktop/src/main.tsx", cx),
            Some(files()[2].path.clone())
        );
    }

    #[gpui::test]
    fn preserves_an_exact_path_even_when_it_is_absent_from_the_project_index(
        cx: &mut TestAppContext,
    ) {
        let (fs, index) = setup(cx);
        let ignored = format!("{CWD}/ignored/App.tsx");
        let task = index.update(cx, |index, cx| {
            index.resolve_file_open_request(CWD, &ignored, true, cx)
        });
        cx.run_until_parked();
        assert_eq!(task.now_or_never(), Some(ignored));
        assert!(fs.list_calls().is_empty());
    }

    #[gpui::test]
    fn returns_the_cached_listing_until_refresh(cx: &mut TestAppContext) {
        let (fs, index) = setup(cx);
        settle(load(&index, false, cx), cx);
        let mut more = files();
        more.push(extra());
        fs.set_files(more.clone());
        assert_eq!(settle(load(&index, false, cx), cx), files());
        assert_eq!(fs.list_calls().len(), 1);
        assert_eq!(settle(load(&index, true, cx), cx), more);
        assert_eq!(
            index
                .read_with(cx, |index, _| index.peek_project_files(CWD))
                .map(|files| files.to_vec()),
            Some(more)
        );
    }

    #[gpui::test]
    fn does_not_drop_a_refresh_that_arrives_while_a_scan_is_in_flight(cx: &mut TestAppContext) {
        let (fs, index) = setup(cx);
        fs.defer_next_files();
        fs.defer_next_files();
        let initial = load(&index, false, cx);
        let refresh = load(&index, true, cx);
        cx.run_until_parked();
        assert_eq!(fs.list_calls().len(), 2);

        fs.resolve_files(files());
        assert_eq!(settle(initial, cx), files());
        assert_eq!(
            index.read_with(cx, |index, _| index.peek_project_files(CWD)),
            None
        );

        let mut more = files();
        more.push(extra());
        fs.resolve_files(more.clone());
        assert_eq!(settle(refresh, cx), more);
        assert_eq!(
            index
                .read_with(cx, |index, _| index.peek_project_files(CWD))
                .map(|files| files.to_vec()),
            Some(more)
        );
    }

    #[gpui::test]
    fn reuses_the_in_flight_scan_when_refresh_is_not_requested(cx: &mut TestAppContext) {
        let (fs, index) = setup(cx);
        fs.defer_next_files();
        let first = load(&index, false, cx);
        let second = load(&index, false, cx);
        cx.run_until_parked();
        assert_eq!(fs.list_calls().len(), 1);
        fs.resolve_files(files());
        assert_eq!(settle(first, cx), files());
        assert_eq!(settle(second, cx), files());
    }

    #[gpui::test]
    fn notifies_subscribers_when_the_listing_changes(cx: &mut TestAppContext) {
        let (_, index) = setup(cx);
        let count = std::rc::Rc::new(std::cell::Cell::new(0));
        let seen = count.clone();
        let _subscription =
            cx.update(|cx| cx.observe(&index, move |_, _| seen.set(seen.get() + 1)));
        settle(load(&index, false, cx), cx);
        assert_eq!(count.get(), 1);
        assert_eq!(
            index
                .read_with(cx, |index, _| index.peek_project_files(CWD))
                .map(|files| files.to_vec()),
            Some(files())
        );
    }

    #[gpui::test]
    fn does_not_drop_a_scan_for_a_different_project(cx: &mut TestAppContext) {
        let (fs, index) = setup(cx);
        let other = "/Users/me/other";
        fs.defer_next_files();
        let scan = index.update(cx, |index, cx| index.load_project_files(other, false, cx));
        index.update(cx, |index, cx| {
            index.invalidate_project_files(Some(CWD), cx)
        });
        cx.run_until_parked();
        fs.resolve_files(files());
        assert_eq!(settle(scan, cx), files());
        assert_eq!(
            index
                .read_with(cx, |index, _| index.peek_project_files(other))
                .map(|files| files.to_vec()),
            Some(files())
        );
    }

    #[gpui::test]
    fn refreshes_the_last_project_after_the_debounce(cx: &mut TestAppContext) {
        let (fs, index) = setup(cx);
        settle(load(&index, false, cx), cx);
        let mut more = files();
        more.push(extra());
        fs.set_files(more.clone());
        index.update(cx, |index, cx| index.schedule_index_refresh(cx));
        cx.run_until_parked();
        assert_eq!(fs.list_calls().len(), 1);
        cx.executor().advance_clock(REFRESH_DELAY);
        cx.run_until_parked();
        assert_eq!(
            index
                .read_with(cx, |index, _| index.peek_project_files(CWD))
                .map(|files| files.to_vec()),
            Some(more)
        );
    }

    #[gpui::test]
    fn keeps_one_listing_per_project(cx: &mut TestAppContext) {
        let (fs, index) = setup(cx);
        let other = "/Users/me/other";
        settle(load(&index, false, cx), cx);
        settle(
            index.update(cx, |index, cx| index.load_project_files(other, false, cx)),
            cx,
        );
        assert_eq!(fs.list_calls().len(), 2);
        // Both projects stay listed, so alternating readers do not re-list.
        settle(load(&index, false, cx), cx);
        settle(
            index.update(cx, |index, cx| index.load_project_files(other, false, cx)),
            cx,
        );
        assert_eq!(fs.list_calls().len(), 2);
        index.read_with(cx, |index, _| {
            assert!(index.peek_project_files(CWD).is_some());
            assert!(index.peek_project_files(other).is_some());
        });
    }

    #[gpui::test]
    fn evicts_the_least_recently_used_listing(cx: &mut TestAppContext) {
        let (_, index) = setup(cx);
        let projects: Vec<String> = (0..=MAX_CACHED_PROJECTS)
            .map(|n| format!("/Users/me/project{n}"))
            .collect();
        for project in &projects {
            settle(
                index.update(cx, |index, cx| index.load_project_files(project, false, cx)),
                cx,
            );
            // Reading the first project keeps it recent.
            index.read_with(cx, |index, _| index.peek_project_files(&projects[0]));
        }
        index.read_with(cx, |index, _| {
            assert!(index.peek_project_files(&projects[0]).is_some());
            assert!(index.peek_project_files(&projects[1]).is_none());
            assert!(
                index
                    .peek_project_files(&projects[MAX_CACHED_PROJECTS])
                    .is_some()
            );
        });
    }

    #[gpui::test]
    fn an_unchanged_rescan_keeps_the_listing_and_notifies_nobody(cx: &mut TestAppContext) {
        let (_, index) = setup(cx);
        let first = settle_arc(load(&index, false, cx), cx);
        let count = std::rc::Rc::new(std::cell::Cell::new(0));
        let seen = count.clone();
        let _observe = cx.update(|cx| cx.observe(&index, move |_, _| seen.set(seen.get() + 1)));
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let log = events.clone();
        let _subscribe = cx.update(|cx| {
            cx.subscribe(&index, move |_, event: &ProjectFilesChanged, _| {
                log.borrow_mut().push(event.cwd.clone())
            })
        });
        let again = settle_arc(load(&index, true, cx), cx);
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(count.get(), 0);
        assert!(events.borrow().is_empty());
    }

    #[gpui::test]
    fn names_the_project_whose_listing_changed(cx: &mut TestAppContext) {
        let (fs, index) = setup(cx);
        settle(load(&index, false, cx), cx);
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let log = events.clone();
        let _subscribe = cx.update(|cx| {
            cx.subscribe(&index, move |_, event: &ProjectFilesChanged, _| {
                log.borrow_mut().push(event.cwd.clone())
            })
        });
        let mut more = files();
        more.push(extra());
        fs.set_files(more);
        settle(load(&index, true, cx), cx);
        assert_eq!(*events.borrow(), vec![CWD.to_string()]);
    }

    #[gpui::test]
    fn an_invalidated_listing_stays_readable_until_the_relist_lands(cx: &mut TestAppContext) {
        let (fs, index) = setup(cx);
        settle(load(&index, false, cx), cx);
        let mut more = files();
        more.push(extra());
        fs.set_files(more.clone());
        index.update(cx, |index, cx| {
            index.invalidate_project_files(Some(CWD), cx)
        });
        assert_eq!(
            index
                .read_with(cx, |index, _| index.peek_project_files(CWD))
                .map(|files| files.to_vec()),
            Some(files())
        );
        // A load after the invalidation waits for a fresh scan.
        assert_eq!(settle(load(&index, false, cx), cx), more);
        assert_eq!(fs.list_calls().len(), 2);
    }

    #[gpui::test]
    fn an_invalidated_listing_is_relisted_after_the_debounce(cx: &mut TestAppContext) {
        let (fs, index) = setup(cx);
        let other = "/Users/me/other";
        settle(
            index.update(cx, |index, cx| index.load_project_files(other, false, cx)),
            cx,
        );
        settle(load(&index, false, cx), cx);
        let mut more = files();
        more.push(extra());
        fs.set_files(more.clone());
        index.update(cx, |index, cx| {
            index.invalidate_project_files(Some(other), cx)
        });
        cx.executor().advance_clock(REFRESH_DELAY);
        cx.run_until_parked();
        assert_eq!(
            index
                .read_with(cx, |index, _| index.peek_project_files(other))
                .map(|files| files.to_vec()),
            Some(more)
        );
    }

    #[gpui::test]
    fn a_failed_scan_answers_until_the_retry_delay(cx: &mut TestAppContext) {
        let (fs, index) = setup(cx);
        fs.fail_files("Project scan unavailable");
        assert!(settle_result(load(&index, false, cx), cx).is_err());
        assert_eq!(fs.list_calls().len(), 1);
        // The failure answers at once instead of starting another scan.
        let again = load(&index, false, cx);
        assert!(again.clone().now_or_never().expect("ready").is_err());
        assert_eq!(fs.list_calls().len(), 1);
        cx.executor().advance_clock(FAILED_SCAN_RETRY);
        fs.set_files(files());
        assert_eq!(settle(load(&index, false, cx), cx), files());
        assert_eq!(fs.list_calls().len(), 2);
    }

    #[gpui::test]
    fn a_folder_that_is_not_a_project_answers_at_once(cx: &mut TestAppContext) {
        let (fs, index) = setup(cx);
        let home = index.update(cx, |index, cx| index.load_project_files("/", false, cx));
        assert!(home.now_or_never().is_some());
        assert!(fs.list_calls().is_empty());
    }

    #[gpui::test]
    fn a_focus_relists_the_last_project_and_the_others_when_read(cx: &mut TestAppContext) {
        let (fs, index) = setup(cx);
        let other = "/Users/me/other";
        settle(
            index.update(cx, |index, cx| index.load_project_files(other, false, cx)),
            cx,
        );
        settle(load(&index, false, cx), cx);
        let mut more = files();
        more.push(extra());
        fs.set_files(more.clone());
        index.update(cx, |index, cx| index.window_shown(cx));
        cx.executor().advance_clock(REFRESH_DELAY);
        cx.run_until_parked();
        // Only the last project re-listed on focus.
        assert_eq!(fs.list_calls().len(), 3);
        let peek = |index: &Entity<FileIndex>, cwd: &str, cx: &mut TestAppContext| {
            index
                .read_with(cx, |index, _| index.peek_project_files(cwd))
                .map(|files| files.to_vec())
        };
        assert_eq!(peek(&index, CWD, cx), Some(more.clone()));
        assert_eq!(peek(&index, other, cx), Some(files()));
        // Reading the other project re-lists it once.
        index.update(cx, |index, cx| index.revalidate(other, cx));
        index.update(cx, |index, cx| index.revalidate(other, cx));
        cx.run_until_parked();
        assert_eq!(fs.list_calls().len(), 4);
        assert_eq!(peek(&index, other, cx), Some(more));
        index.update(cx, |index, cx| index.revalidate(other, cx));
        cx.run_until_parked();
        assert_eq!(fs.list_calls().len(), 4);
    }

    #[gpui::test]
    fn a_failed_relist_drops_the_outdated_listing(cx: &mut TestAppContext) {
        let (fs, index) = setup(cx);
        settle(load(&index, false, cx), cx);
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let log = events.clone();
        let _subscribe = cx.update(|cx| {
            cx.subscribe(&index, move |_, event: &ProjectFilesChanged, _| {
                log.borrow_mut().push(event.cwd.clone())
            })
        });
        fs.fail_files("worktree removed");
        index.update(cx, |index, cx| {
            index.invalidate_project_files(Some(CWD), cx)
        });
        cx.executor().advance_clock(REFRESH_DELAY);
        cx.run_until_parked();
        assert_eq!(
            index.read_with(cx, |index, _| index.peek_project_files(CWD)),
            None
        );
        assert_eq!(*events.borrow(), vec![CWD.to_string()]);
    }

    #[test]
    fn ranking_keeps_the_full_sort_order_when_it_cuts_early() {
        let names = [
            "b.rs", "a.rs", "ab.rs", "ba.rs", "src/a.rs", "src/b.rs", "x/a.rs", "a.rs",
        ];
        let files: Vec<ProjectFile> = names
            .iter()
            .enumerate()
            .map(|(n, relative)| {
                let name = relative.rsplit('/').next().unwrap();
                ProjectFile::new(name, format!("{CWD}/{n}/{relative}"), *relative)
            })
            .collect();
        let all = rank_project_files_limit(&files, "a", &[], usize::MAX);
        for limit in 0..=files.len() {
            let cut = rank_project_files_limit(&files, "a", &[], limit);
            assert_eq!(cut, all[..limit.min(all.len())].to_vec(), "limit {limit}");
        }
        // Equal rows keep listing order, as a stable sort did.
        let twins: Vec<&RankedFile> = all
            .iter()
            .filter(|hit| hit.file.relative == "a.rs")
            .collect();
        assert_eq!(twins.len(), 2);
        assert!(twins[0].file.path < twins[1].file.path);
    }

    #[test]
    fn ranks_recents_without_a_query_and_fuzzy_matches_with_one() {
        let recents = vec![files()[2].path.clone(), "/gone.ts".into()];
        let ranked = rank_project_files(&files(), "", &recents);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].file, files()[2]);
        let ranked = rank_project_files(&files(), "main", &[]);
        assert_eq!(ranked[0].file, files()[2]);
        // A recent file outranks an equal match.
        let recents = vec![files()[0].path.clone()];
        let ranked = rank_project_files(&files(), "App.tsx", &recents);
        assert_eq!(ranked[0].file, files()[0]);
    }
}
