use super::*;
use monocode_engine::history::sidebar::{ReminderDue, SessionListInput};
use monocode_engine::runtime::session_history::LiveRun;
use monocode_engine::runtime::session_store::SessionSummary;
use monocode_engine::workspace::WorktreeFocus;
use monocode_layout::paths::{is_remote_project_path, project_name};

/// What the list was built from. Building it filters, sorts, and groups
/// every row of the project, so `data` rebuilds only when one of these
/// changed. Inputs read through observed entities count in `observed`; the
/// open sessions, settings, projects, and catalog count in `revision`.
#[derive(PartialEq)]
pub(super) struct ListKey {
    revision: u64,
    observed: u64,
    cwd: String,
    history: Option<gpui::EntityId>,
    active_session_id: Option<String>,
    branch: Option<String>,
    focus: Option<WorktreeFocus>,
    reminders: Vec<ReminderDue>,
    runs: Vec<LiveRun>,
}

impl SessionList {
    pub(super) fn history(&self, cx: &App) -> Option<Entity<monocode_engine::history::History>> {
        self.shell.upgrade()?.read(cx).history.clone()
    }

    pub(super) fn sync_observations(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(history) = self.history(cx)
            && self
                .history_observation
                .as_ref()
                .is_none_or(|(id, _)| *id != history.entity_id())
        {
            self.history_observation = Some((
                history.entity_id(),
                cx.observe(&history, |this, _, cx| {
                    this.observed += 1;
                    cx.notify();
                }),
            ));
        }
        if let Some(history) = self.history(cx) {
            let query = history.read(cx).sidebar().search_query.clone();
            if self.session_search.read(cx).value().as_ref() != query.as_str() {
                self.session_search
                    .update(cx, |input, cx| input.set_value(query, window, cx));
            }
        }
        let cwd = self
            .shell
            .upgrade()
            .map(|shell| shell.read(cx).sidebar_cwd(cx))
            .unwrap_or_default();
        if self
            .remote_watch
            .as_ref()
            .is_some_and(|(path, _)| path != &cwd)
        {
            self.remote_watch = None;
        }
        if is_remote_project_path(&cwd)
            && self.remote_watch.is_none()
            && let Some(remote) = monocode_engine::remote::RemoteGlobal::try_global(cx)
        {
            let connections = remote.connections.clone();
            let watch = connections.update(cx, |connections, cx| {
                connections.watch_project_sessions(&cwd, cx)
            });
            self.remote_watch = Some((cwd, watch));
        }
    }

    /// The list for this frame: the last one when nothing it reads changed.
    pub(super) fn data(&mut self, cx: &mut App) -> Rc<ListData> {
        let Some(shell) = self.shell.upgrade() else {
            return Rc::default();
        };
        let (cwd, workspace, history) = {
            let shell = shell.read(cx);
            (
                shell.sidebar_cwd(cx),
                shell.workspace.clone(),
                shell.history.clone(),
            )
        };
        if Engine::try_global(cx).is_none() {
            return Rc::default();
        }
        let remote_project = is_remote_project_path(&cwd);
        let active_session_id = workspace.as_ref().and_then(|workspace| {
            workspace
                .read(cx)
                .active_session_ref(cx)
                .map(|s| s.id.clone())
        });
        let branch = monocode_engine::projects::ProjectsGlobal::try_global(cx)
            .and_then(|projects| projects.git.read(cx).get(&cwd))
            .and_then(|status| {
                status
                    .read(cx)
                    .index()
                    .and_then(|index| index.branch.clone())
            });
        // The sidebar shows only the focused worktree's sessions.
        let focus = if remote_project {
            None
        } else {
            workspace
                .as_ref()
                .and_then(|workspace| workspace.read(cx).worktree_focus(&cwd).cloned())
        };
        let reminders = monocode_engine::automations::AutomationsPackage::try_global(cx)
            .map(|package| {
                package
                    .reminders
                    .read(cx)
                    .reminders()
                    .iter()
                    .filter(|reminder| {
                        monocode_layout::paths::same_project_path(&reminder.cwd, &cwd)
                    })
                    .map(|reminder| ReminderDue {
                        session_id: reminder.session_id.clone(),
                        due_at: reminder.due_at,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let runs = match (&history, remote_project) {
            (Some(history), false) => history.read(cx).host().live_runs(cx),
            _ => Vec::new(),
        };
        let key = ListKey {
            revision: crate::revisions::revision(cx),
            observed: self.observed,
            cwd,
            history: history.as_ref().map(|history| history.entity_id()),
            active_session_id,
            branch,
            focus,
            reminders,
            runs,
        };
        if let Some((built, data)) = &self.data_cache
            && *built == key
        {
            return data.clone();
        }
        let data = Rc::new(build_list_data(&key, workspace.as_ref(), history, cx));
        self.data_cache = Some((key, data.clone()));
        data
    }
}

/// Build the list from the engine. Port of the sessions half of
/// Sidebar.tsx's render.
fn build_list_data(
    key: &ListKey,
    workspace: Option<&Entity<monocode_engine::workspace::Workspace>>,
    history: Option<Entity<monocode_engine::history::History>>,
    cx: &mut App,
) -> ListData {
    let mut data = ListData::default();
    let cwd = key.cwd.as_str();
    let sessions = Engine::sessions(cx);
    let mut busy = sessions.read(cx).busy_session_ids().clone();
    let (mut approvals, mut unseen) = Attention::try_global(cx)
        .map(|attention| {
            (
                attention.approvals.read(cx).approval_session_ids().clone(),
                attention.notifier.read(cx).unseen_finished_ids().clone(),
            )
        })
        .unwrap_or_default();
    if workspace.is_some() {
        data.active_session_id = key.active_session_id.clone();
    }
    let local_active_session_id = data.active_session_id.clone();
    let Some(history) = history else { return data };
    let remote_project = is_remote_project_path(cwd);
    let mut remote_loaded = false;
    let branch = key.branch.clone();
    let (rows, open_rows) = {
        // Borrow the open sessions: a copy would clone every transcript.
        let app: &App = cx;
        let open = sessions.read(app).all();
        if remote_project {
            let remote = monocode_engine::remote::RemoteGlobal::try_global(app);
            let project = remote.map(|remote| remote.connections.read(app).project_sessions(cwd));
            let mut rows = Vec::new();
            if let Some(project) = project {
                remote_loaded = project.loaded;
                for host in project.sessions {
                    if host.status == monocode_remote::host::protocol::HostSessionStatus::Running {
                        busy.insert(host.id.clone());
                    }
                    if host.needs_input == Some(true) {
                        approvals.insert(host.id.clone());
                    }
                    let mut row = SessionSummary::new(&host.id, cwd, host.harness);
                    row.title = host.title;
                    row.model = host.model.unwrap_or_default();
                    row.runtime_mode = host.runtime_mode.unwrap_or_default();
                    row.created_at = host.created_at.unwrap_or(host.updated_at);
                    row.updated_at = host.updated_at;
                    row.archived = host.archived;
                    row.pinned = host.pinned;
                    row.provider_session_id = host.provider_session_id;
                    row.branch = host.branch;
                    row.worktree_cwd = host.worktree_cwd;
                    row.repo = host.repo.or_else(|| Some(project_name(cwd)));
                    row.draft = host.draft;
                    row.linked_work_item = host.linked_work_item;
                    rows.push(row);
                }
            }
            if let Some(remote) = remote {
                let connections = remote.connections.read(app);
                for session in open
                    .iter()
                    .filter(|session| monocode_layout::paths::same_project_path(&session.cwd, cwd))
                {
                    if let Some(host_id) = connections.remote_session_for(&session.id)
                        && unseen.contains(&session.id)
                    {
                        unseen.insert(host_id);
                    }
                }
                data.active_session_id = data
                    .active_session_id
                    .as_ref()
                    .and_then(|id| connections.remote_session_for(id));
            }
            (rows, Vec::new())
        } else {
            let history = history.read(app);
            (
                history.sidebar_history(open, branch.as_deref(), &key.runs),
                history.open_project_sessions(open, branch.as_deref()),
            )
        }
    };
    let focus = key.focus.as_ref();
    let in_focus = |row: &SessionSummary| {
        monocode_engine::workspace::in_worktree_focus(&row.cwd, row.worktree_cwd.as_deref(), focus)
    };
    let rows: Vec<SessionSummary> = rows.into_iter().filter(|row| in_focus(row)).collect();
    let open_rows: Vec<SessionSummary> =
        open_rows.into_iter().filter(|row| in_focus(row)).collect();
    let reminders = &key.reminders;
    let pending = history.read(cx).is_pending();
    let failed = history.read(cx).has_failed();
    let input = SessionListInput {
        project_sessions: &rows,
        open_sessions: &open_rows,
        busy_ids: &busy,
        approval_ids: &approvals,
        unseen_finished_ids: &unseen,
        reminders,
        active_listed_session_id: data.active_session_id.as_deref(),
        active_session_id: local_active_session_id.as_deref(),
        remote: remote_project,
        listing_pending: pending,
        listing_failed: failed,
        remote_loaded,
    };
    let view = history.update(cx, |history, cx| {
        let view = history.session_list(&input);
        history.sync_session_list(&view, &input, cx);
        view
    });
    data.sessions_loading = if remote_project {
        !remote_loaded && rows.is_empty()
    } else {
        pending
    };
    data.selected_ids = history.read(cx).sidebar().selected.clone();
    data.entries = view.entries;
    data.has_more = view.has_more;
    data.filters_active = view.filters_active;
    data.search_narrowed = view.search_narrowed;
    data.harnesses = view.harnesses;
    data.listed = view.listed;
    data.navigation_ids = view.mounted_navigation_ids;
    data.sessions = view
        .visible
        .iter()
        .map(|row| {
            let status = if approvals.contains(&row.id) {
                SessionStatus::NeedsApproval
            } else if busy.contains(&row.id) {
                SessionStatus::Busy
            } else if unseen.contains(&row.id) {
                SessionStatus::Done
            } else if row.draft == Some(true) {
                SessionStatus::Draft
            } else {
                SessionStatus::Idle
            };
            SessionCard {
                provider: format::provider_logo(row.harness),
                model: model_name(row.harness, &row.model, cx),
                title: session_display_title(&row.title, row.harness),
                git: if row.worktree_removed == Some(true) {
                    NO_BRANCH_LABEL.into()
                } else {
                    format_git_label(row.repo.as_deref(), row.branch.as_deref())
                },
                additions: row.additions.unwrap_or(0),
                deletions: row.deletions.unwrap_or(0),
                updated_at: row.updated_at,
                created_at: row.created_at,
                status,
                pinned: row.pinned == Some(true),
                id: row.id.clone(),
            }
        })
        .collect();
    data.card_index = data
        .sessions
        .iter()
        .enumerate()
        .map(|(index, card)| (card.id.clone(), index))
        .collect();
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::ShellOptions;
    use gpui::TestAppContext;
    use monocode_core::{Block, BlockRole, Session};
    use monocode_engine::history::History;
    use monocode_engine::runtime::testing::init_test_engine;
    use monocode_engine::workspace::WorkspaceConfig;

    #[gpui::test]
    fn streamed_text_reuses_the_list_and_a_new_title_rebuilds_it(cx: &mut TestAppContext) {
        cx.skip_drawing();
        init_test_engine(cx);
        cx.update(|cx| {
            gpui_component::init(cx);
            monocode_ui::init(monocode_ui::AppearanceSettings::default(), cx);
            Engine::sessions(cx).update(cx, |sessions, cx| {
                let mut session = Session::blank("one", HarnessId::Codex, "model", "/repo");
                session.blocks.push(Block::new("u", BlockRole::User, "Hi"));
                session
                    .blocks
                    .push(Block::new("a", BlockRole::Assistant, "Hel"));
                session.busy = Some(true);
                sessions.insert(session, cx);
            });
        });
        let shell = cx.add_window(|window, cx| {
            let mut shell = Shell::new(ShellOptions::full(), window, cx);
            let history = cx.new(|cx| History::new(monocode_settings::Kv::in_memory(), cx));
            shell.attach(WorkspaceConfig::fresh(Some("/repo")), history, window, cx);
            shell
        });
        let list = shell
            .update(cx, |_, window, cx| {
                let weak = cx.weak_entity();
                cx.new(|cx| SessionList::new(weak, None, window, cx))
            })
            .unwrap();
        cx.run_until_parked();
        let data = |cx: &mut TestAppContext| list.update(cx, |list, cx| list.data(cx));
        let first = data(cx);
        assert!(first.sessions.iter().any(|card| card.id == "one"));
        assert!(Rc::ptr_eq(&first, &data(cx)));

        // A streamed token changes the transcript and nothing the list shows.
        cx.update(|cx| {
            Engine::sessions(cx).update(cx, |sessions, cx| {
                sessions.update("one", cx, |session| session.blocks[1].text.push_str("lo"));
            })
        });
        cx.run_until_parked();
        assert!(Rc::ptr_eq(&first, &data(cx)));

        cx.update(|cx| {
            Engine::sessions(cx).update(cx, |sessions, cx| {
                sessions.update("one", cx, |session| session.title = "Renamed".into());
            })
        });
        cx.run_until_parked();
        let renamed = data(cx);
        assert!(!Rc::ptr_eq(&first, &renamed));
        let card = renamed.card_index["one"];
        assert_eq!(renamed.sessions[card].title, "Renamed");
    }

    /// The cards' "now" and "3m" labels move with the history's sidebar
    /// clock: each tick notifies the history, and the list redraws (and the
    /// cached sidebar with it), so the list needs no ticker of its own.
    #[gpui::test]
    fn the_sidebar_clock_redraws_the_list(cx: &mut TestAppContext) {
        cx.skip_drawing();
        init_test_engine(cx);
        cx.update(|cx| {
            gpui_component::init(cx);
            monocode_ui::init(monocode_ui::AppearanceSettings::default(), cx);
        });
        let shell = cx.add_window(|window, cx| {
            let mut shell = Shell::new(ShellOptions::full(), window, cx);
            let history = cx.new(|cx| History::new(monocode_settings::Kv::in_memory(), cx));
            shell.attach(WorkspaceConfig::fresh(Some("/repo")), history, window, cx);
            shell
        });
        let list = shell
            .update(cx, |_, window, cx| {
                let weak = cx.weak_entity();
                cx.new(|cx| SessionList::new(weak, None, window, cx))
            })
            .unwrap();
        cx.run_until_parked();
        list.update(cx, |list, cx| list.data(cx));
        let before = list.read_with(cx, |list, _| list.observed);
        cx.executor()
            .advance_clock(monocode_engine::history::sidebar::SIDEBAR_CLOCK_INTERVAL);
        cx.run_until_parked();
        assert!(list.read_with(cx, |list, _| list.observed) > before);
    }
}
