//! A change counter for render-time caches in the shell.
//!
//! Any change redraws the whole window and every view renders again. While
//! an agent streams, that happens up to once per display frame, and a busy
//! spinner keeps it going between events. The sidebar, the project rail, and
//! the title bar build their rows from every open session, the settings
//! store, the projects package, and the model catalog. The counter here moves
//! only when one of those changes in a way the rows can show, so the regions
//! rebuild their rows then and reuse them on every other frame.
//!
//! Streamed text does not move it: [`current_sessions_digest`] hashes what the shell
//! regions read from a session (title, model, busy state, branch, and so on)
//! and skips the transcript text.

use std::any::Any;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{App, AppContext as _, Entity, Global, Subscription, Task};
use monocode_app::boot::AppServices;
use monocode_core::session::{session_draft_block, session_needs_input};
use monocode_core::{BlockRole, Session};
use monocode_engine::projects::ProjectsGlobal;
use monocode_engine::runtime::Engine;

/// A hash of what the shell regions read from one session: the fields the
/// sidebar cards, the rail's busy marks, and the title bar tabs show, plus
/// the block facts they derive (a user turn, a draft, a pending approval).
/// Transcript text is left out, so streaming does not change it.
fn session_hash(session: &Session) -> u64 {
    let mut hasher = DefaultHasher::new();
    session.id.hash(&mut hasher);
    session.title.hash(&mut hasher);
    session.harness.hash(&mut hasher);
    session.model.hash(&mut hasher);
    session.runtime_mode.hash(&mut hasher);
    session.cwd.hash(&mut hasher);
    session.busy.hash(&mut hasher);
    session.branch.hash(&mut hasher);
    session.worktree_cwd.hash(&mut hasher);
    session.worktree_removed.hash(&mut hasher);
    session.orchestration_lead_id.hash(&mut hasher);
    session.inbox_ask.is_some().hash(&mut hasher);
    session.provider_session_id.hash(&mut hasher);
    session.provider_account_id.hash(&mut hasher);
    session.automation_id.hash(&mut hasher);
    session
        .linked_work_item
        .as_ref()
        .map(|item| (&item.url, &item.repo, item.number))
        .hash(&mut hasher);
    session.pending_question.is_some().hash(&mut hasher);
    session.blocks.is_empty().hash(&mut hasher);
    session
        .blocks
        .iter()
        .any(|block| block.role == BlockRole::User)
        .hash(&mut hasher);
    session_draft_block(&session.blocks)
        .is_some()
        .hash(&mut hasher);
    session_needs_input(session).hash(&mut hasher);
    hasher.finish()
}

/// [`session_hash`] over a list, for tests.
#[cfg(test)]
fn sessions_digest(sessions: &[Session]) -> u64 {
    let mut hasher = DefaultHasher::new();
    sessions.len().hash(&mut hasher);
    for session in sessions {
        session_hash(session).hash(&mut hasher);
    }
    hasher.finish()
}

/// The digest of the open sessions: a hash of [`session_hash`] for each, in
/// order. A session's hash is kept per `Sessions::session_revision`, so a
/// call rehashes only the sessions that changed since the last one.
pub(crate) fn current_sessions_digest(cx: &mut App) -> u64 {
    let Some(sessions) = Engine::try_global(cx).map(|engine| engine.sessions.clone()) else {
        return 0;
    };
    ensure_global(cx);
    let mut kept = std::mem::take(&mut cx.global_mut::<Revisions>().session_hashes);
    let mut hasher = DefaultHasher::new();
    let mut next = std::collections::HashMap::with_capacity(kept.len());
    {
        let sessions = sessions.read(cx);
        sessions.all().len().hash(&mut hasher);
        for session in sessions.all() {
            let revision = sessions.session_revision(&session.id);
            let hash = match kept.remove(&session.id) {
                Some((seen, hash)) if seen == revision => hash,
                _ => session_hash(session),
            };
            hash.hash(&mut hasher);
            next.insert(session.id.clone(), (revision, hash));
        }
    }
    cx.global_mut::<Revisions>().session_hashes = next;
    hasher.finish()
}

/// Notifies after the counter moves. Cached views observe it so they redraw
/// when something they show from the sessions, settings, projects, or
/// catalog changed (see [`observe`]).
pub(crate) struct RevisionSignal;

/// The counter and the listeners that move it.
struct Revisions {
    /// Bumped by the settings store and catalog listeners, which run off
    /// the UI thread or without an `App`.
    external: Arc<AtomicU64>,
    /// Bumped on the UI thread by the sessions and projects observers.
    local: u64,
    sessions_digest: Option<u64>,
    /// [`session_hash`] per session id, with the revision it was taken at.
    session_hashes: std::collections::HashMap<String, (u64, u64)>,
    signal: Entity<RevisionSignal>,
    sessions: Option<Subscription>,
    projects: Option<Subscription>,
    settings: Option<monocode_settings::Subscription>,
    catalog: Option<CatalogListener>,
    /// Forwards the off-thread listeners' bumps to the signal.
    _forward: Option<Task<()>>,
    /// [`memo`] values by key, with the revision they were built at.
    memos: std::cell::RefCell<std::collections::HashMap<&'static str, Memo>>,
}

/// One [`memo`] value and the revision it was built at.
type Memo = (u64, Rc<dyn Any>);

impl Global for Revisions {}

struct CatalogListener {
    catalog: monocode_harness::core::catalog::SharedCatalog,
    id: u64,
}

impl Drop for CatalogListener {
    fn drop(&mut self) {
        self.catalog.unsubscribe(self.id);
    }
}

/// Install the listeners that exist now. Packages that start later get
/// their listener on the next call, so callers may run this on every
/// render.
fn ensure_global(cx: &mut App) {
    if !cx.has_global::<Revisions>() {
        let signal = cx.new(|_| RevisionSignal);
        cx.set_global(Revisions {
            external: Arc::default(),
            local: 0,
            sessions_digest: None,
            session_hashes: Default::default(),
            signal,
            sessions: None,
            projects: None,
            settings: None,
            catalog: None,
            _forward: None,
            memos: Default::default(),
        });
    }
}

fn ensure(cx: &mut App) {
    ensure_global(cx);
    let (has_sessions, has_projects, has_settings) = {
        let revisions = cx.global::<Revisions>();
        (
            revisions.sessions.is_some(),
            revisions.projects.is_some(),
            revisions.settings.is_some(),
        )
    };
    if !has_sessions && let Some(engine) = Engine::try_global(cx) {
        let sessions = engine.sessions.clone();
        let digest = current_sessions_digest(cx);
        let subscription = cx.observe(&sessions, |_, cx| {
            let digest = current_sessions_digest(cx);
            let revisions = cx.global_mut::<Revisions>();
            if revisions.sessions_digest != Some(digest) {
                revisions.sessions_digest = Some(digest);
                revisions.local += 1;
                let signal = revisions.signal.entity_id();
                cx.notify(signal);
            }
        });
        let revisions = cx.global_mut::<Revisions>();
        revisions.sessions_digest = Some(digest);
        revisions.local += 1;
        revisions.sessions = Some(subscription);
    }
    if !has_projects && let Some(projects) = ProjectsGlobal::try_global(cx) {
        let projects = projects.projects.clone();
        let subscription = cx.observe(&projects, |_, cx| {
            let revisions = cx.global_mut::<Revisions>();
            revisions.local += 1;
            let signal = revisions.signal.entity_id();
            cx.notify(signal);
        });
        let revisions = cx.global_mut::<Revisions>();
        revisions.local += 1;
        revisions.projects = Some(subscription);
    }
    if !has_settings && let Some(services) = AppServices::try_global(cx) {
        let (kv, catalog) = (services.kv.clone(), services.catalog.clone());
        let external = cx.global::<Revisions>().external.clone();
        // The listeners bump the counter at once, so a render right after a
        // change reads the new value, and wake the forwarder, which tells the
        // cached views. A full channel already holds a wake-up.
        let (wake, woken) = async_channel::bounded::<()>(1);
        let (counter, settings_wake) = (external.clone(), wake.clone());
        // Composer drafts save on each keystroke and no shell region shows
        // them.
        let settings = kv.subscribe(move |change| {
            if !change.key.contains("draft") {
                counter.fetch_add(1, Ordering::Relaxed);
                let _ = settings_wake.try_send(());
            }
        });
        let counter = external.clone();
        let id = catalog.subscribe(move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
            let _ = wake.try_send(());
        });
        let forward = cx.spawn(async move |cx| {
            while woken.recv().await.is_ok() {
                cx.update(|cx| {
                    if let Some(revisions) = cx.try_global::<Revisions>() {
                        let signal = revisions.signal.entity_id();
                        cx.notify(signal);
                    }
                });
            }
        });
        let revisions = cx.global_mut::<Revisions>();
        revisions.local += 1;
        revisions.settings = Some(settings);
        revisions.catalog = Some(CatalogListener { catalog, id });
        revisions._forward = Some(forward);
    }
}

/// The current revision of the open sessions' metadata, the settings store,
/// the projects package, and the model catalog. Equal values mean none of
/// them changed in a way the shell regions show.
pub(crate) fn revision(cx: &mut App) -> u64 {
    ensure(cx);
    let revisions = cx.global::<Revisions>();
    // Both parts only grow, so their sum moves whenever either moves.
    revisions
        .local
        .wrapping_add(revisions.external.load(Ordering::Relaxed))
}

/// Call `on_change` on `view` after the [`revision`] moves. Views drawn
/// through `Entity::cached` use it to redraw when the sessions' metadata,
/// the settings, the projects, or the catalog change.
pub(crate) fn observe<V: 'static>(
    cx: &mut gpui::Context<V>,
    on_change: impl Fn(&mut V, &mut gpui::Context<V>) + 'static,
) -> Subscription {
    ensure(cx);
    let signal = cx.global::<Revisions>().signal.clone();
    cx.observe(&signal, move |view, _, cx| on_change(view, cx))
}

/// [`observe`] for views that need their window when they update.
pub(crate) fn observe_in<V: 'static>(
    window: &mut gpui::Window,
    cx: &mut gpui::Context<V>,
    on_change: impl Fn(&mut V, &mut gpui::Window, &mut gpui::Context<V>) + 'static,
) -> Subscription {
    ensure(cx);
    let signal = cx.global::<Revisions>().signal.clone();
    cx.observe_in(&signal, window, move |view, _, window, cx| {
        on_change(view, window, cx)
    })
}

/// [`revision`] for callers that hold only `&App`. `None` until a window
/// started the counter, and callers should not cache then.
pub(crate) fn current(cx: &App) -> Option<u64> {
    let revisions = cx.try_global::<Revisions>()?;
    // The listeners install lazily; without them the counter does not move.
    if revisions.sessions.is_none() || revisions.projects.is_none() || revisions.settings.is_none()
    {
        return None;
    }
    Some(
        revisions
            .local
            .wrapping_add(revisions.external.load(Ordering::Relaxed)),
    )
}

/// A value built from the settings store, the projects, the catalog, or
/// the sessions' metadata, kept until the [`revision`] moves. Adapters that
/// view crates call while drawing use it to parse a settings record once
/// instead of on every call. Builds every time while the counter is not
/// running.
pub(crate) fn memo<T: 'static>(
    cx: &App,
    key: &'static str,
    build: impl FnOnce(&App) -> T,
) -> Rc<T> {
    let Some(revision) = current(cx) else {
        return Rc::new(build(cx));
    };
    let revisions = cx.global::<Revisions>();
    let kept = revisions
        .memos
        .borrow()
        .get(key)
        .filter(|(seen, _)| *seen == revision)
        .map(|(_, value)| value.clone());
    if let Some(value) = kept.and_then(|value| value.downcast::<T>().ok()) {
        return value;
    }
    let value = Rc::new(build(cx));
    revisions
        .memos
        .borrow_mut()
        .insert(key, (revision, value.clone() as Rc<dyn Any>));
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use monocode_core::HarnessId;
    use monocode_engine::runtime::testing::init_test_engine;

    #[test]
    fn streamed_text_leaves_the_digest_alone() {
        let mut session = Session::blank("one", HarnessId::Codex, "model", "/repo");
        let before = sessions_digest(std::slice::from_ref(&session));
        session.busy = Some(true);
        let busy = sessions_digest(std::slice::from_ref(&session));
        assert_ne!(before, busy);
        session.blocks.push(monocode_core::Block::new(
            "a",
            BlockRole::Assistant,
            "Streaming",
        ));
        let first = sessions_digest(std::slice::from_ref(&session));
        session.blocks[0].text.push_str(" more text");
        assert_eq!(first, sessions_digest(std::slice::from_ref(&session)));
        session.title = "Renamed".into();
        assert_ne!(first, sessions_digest(std::slice::from_ref(&session)));
    }

    #[gpui::test]
    fn the_revision_moves_on_metadata_and_not_on_text(cx: &mut TestAppContext) {
        init_test_engine(cx);
        let start = cx.update(|cx| {
            Engine::sessions(cx).update(cx, |sessions, cx| {
                let mut session = Session::blank("one", HarnessId::Codex, "model", "/repo");
                session.blocks.push(monocode_core::Block::new(
                    "a",
                    BlockRole::Assistant,
                    "Streaming",
                ));
                sessions.insert(session, cx);
            });
            revision(cx)
        });
        cx.run_until_parked();
        let after_insert = cx.update(revision);
        cx.update(|cx| {
            Engine::sessions(cx).update(cx, |sessions, cx| {
                sessions.update("one", cx, |session| session.blocks[0].text.push('!'));
            })
        });
        cx.run_until_parked();
        assert_eq!(after_insert, cx.update(revision));
        cx.update(|cx| {
            Engine::sessions(cx).update(cx, |sessions, cx| {
                sessions.update("one", cx, |session| session.busy = Some(true));
            })
        });
        cx.run_until_parked();
        let busy = cx.update(revision);
        assert_ne!(after_insert, busy);
        assert!(busy != start);
    }
}
