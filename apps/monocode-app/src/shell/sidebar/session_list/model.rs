use super::*;
use monocode_engine::history::sidebar::{ReminderDue, SessionListInput};
use monocode_engine::runtime::session_store::SessionSummary;
use monocode_layout::paths::{is_remote_project_path, project_name};
use monocode_remote::host::protocol::{HostSessionStatus, HostSessionSummary};
use std::collections::{HashMap, HashSet};

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
                cx.observe(&history, |_, _, cx| cx.notify()),
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
        let locations = crate::machines::project_locations(&cwd, cx);
        let remote_paths: Vec<String> = if locations.len() > 1 {
            locations
                .iter()
                .filter(|location| location.remote)
                .map(|location| location.path.clone())
                .collect()
        } else if is_remote_project_path(&cwd) {
            vec![cwd.clone()]
        } else {
            Vec::new()
        };
        if locations.len() > 1
            && let Some(history) = self.history(cx)
        {
            for location in locations.iter().filter(|location| !location.remote) {
                history.update(cx, |history, cx| history.load_location(&location.path, cx));
            }
        }
        self.remote_watches
            .retain(|(path, _)| remote_paths.contains(path));
        if let Some(remote) = monocode_engine::remote::RemoteGlobal::try_global(cx) {
            let connections = remote.connections.clone();
            for path in remote_paths {
                if self
                    .remote_watches
                    .iter()
                    .any(|(watched, _)| *watched == path)
                {
                    continue;
                }
                let watch = connections.update(cx, |connections, cx| {
                    connections.watch_project_sessions(&path, cx)
                });
                self.remote_watches.push((path, watch));
            }
        }
    }

    pub(super) fn data(&self, cx: &mut App) -> ListData {
        let mut data = ListData::default();
        let Some(shell) = self.shell.upgrade() else {
            return data;
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
            return data;
        }
        let sessions = Engine::sessions(cx);
        let open = sessions.read(cx).all().to_vec();
        let mut busy = sessions.read(cx).busy_session_ids().clone();
        let (mut approvals, mut unseen) = Attention::try_global(cx)
            .map(|attention| {
                (
                    attention.approvals.read(cx).approval_session_ids().clone(),
                    attention.notifier.read(cx).unseen_finished_ids().clone(),
                )
            })
            .unwrap_or_default();
        if let Some(workspace) = &workspace {
            data.active_session_id = workspace.read(cx).active_session(cx).map(|s| s.id);
        }
        let local_active_session_id = data.active_session_id.clone();
        let Some(history) = history else { return data };
        let remote_project = is_remote_project_path(&cwd);
        let mut remote_loaded = false;
        let branch = monocode_engine::projects::ProjectsGlobal::try_global(cx)
            .and_then(|projects| projects.git.read(cx).get(&cwd))
            .and_then(|status| {
                status
                    .read(cx)
                    .index()
                    .and_then(|index| index.branch.clone())
            });
        let locations = crate::machines::project_locations(&cwd, cx);
        let merged = locations.len() > 1;
        let mut machines: HashMap<String, String> = HashMap::new();
        let mut local_pending = false;
        let (rows, open_rows) = if merged {
            // One project on several machines: every location's sessions,
            // each row keeping its own location as `cwd`.
            let runs = history.read(cx).host().live_runs(cx);
            let remote = monocode_engine::remote::RemoteGlobal::try_global(cx);
            let mut groups = Vec::new();
            let mut open_rows = Vec::new();
            remote_loaded = true;
            for location in &locations {
                if location.remote {
                    let Some(remote) = remote else { continue };
                    let project = remote.connections.read(cx).project_sessions(&location.path);
                    remote_loaded &= project.loaded;
                    let rows = project
                        .sessions
                        .into_iter()
                        .map(|host| {
                            note_host_status(&host, &mut busy, &mut approvals);
                            host_row(host, &location.path)
                        })
                        .collect();
                    note_unseen_hosts(&open, &location.path, &mut unseen, cx);
                    groups.push((location.machine.clone(), rows));
                } else {
                    let branch = location_branch(&location.path, cx);
                    let history = history.read(cx);
                    local_pending |= history.location_pending(&location.path);
                    groups.push((
                        location.machine.clone(),
                        history.location_history(&location.path, &open, branch.as_deref(), &runs),
                    ));
                    open_rows.extend(history.open_location_sessions(
                        &location.path,
                        &open,
                        branch.as_deref(),
                    ));
                }
            }
            if remote_project && let Some(remote) = remote {
                let connections = remote.connections.read(cx);
                data.active_session_id = data
                    .active_session_id
                    .as_ref()
                    .and_then(|id| connections.remote_session_for(id));
            }
            let (rows, labels) = merge_location_rows(groups);
            machines = labels;
            (rows, open_rows)
        } else if remote_project {
            let remote = monocode_engine::remote::RemoteGlobal::try_global(cx);
            let project = remote.map(|remote| remote.connections.read(cx).project_sessions(&cwd));
            let mut rows = Vec::new();
            if let Some(project) = project {
                remote_loaded = project.loaded;
                for host in project.sessions {
                    note_host_status(&host, &mut busy, &mut approvals);
                    rows.push(host_row(host, &cwd));
                }
            }
            if let Some(remote) = remote {
                note_unseen_hosts(&open, &cwd, &mut unseen, cx);
                let connections = remote.connections.read(cx);
                data.active_session_id = data
                    .active_session_id
                    .as_ref()
                    .and_then(|id| connections.remote_session_for(id));
            }
            (rows, Vec::new())
        } else {
            let runs = history.read(cx).host().live_runs(cx);
            let history = history.read(cx);
            (
                history.sidebar_history(&open, branch.as_deref(), &runs),
                history.open_project_sessions(&open, branch.as_deref()),
            )
        };
        // The sidebar shows only the focused worktree's sessions.
        let focus = if remote_project {
            None
        } else {
            workspace
                .as_ref()
                .and_then(|workspace| workspace.read(cx).worktree_focus(&cwd).cloned())
        };
        // Another machine's sessions are not in this folder's worktrees.
        let in_focus = |row: &SessionSummary| {
            !monocode_layout::paths::same_project_path(&row.cwd, &cwd)
                || monocode_engine::workspace::in_worktree_focus(
                    &row.cwd,
                    row.worktree_cwd.as_deref(),
                    focus.as_ref(),
                )
        };
        let rows: Vec<SessionSummary> = rows.into_iter().filter(|row| in_focus(row)).collect();
        let open_rows: Vec<SessionSummary> =
            open_rows.into_iter().filter(|row| in_focus(row)).collect();
        let reminders = monocode_engine::automations::AutomationsPackage::try_global(cx)
            .map(|package| {
                package
                    .reminders
                    .read(cx)
                    .reminders()
                    .iter()
                    .filter(|reminder| {
                        monocode_layout::paths::same_project_path(&reminder.cwd, &cwd)
                            || locations.iter().any(|location| {
                                !location.remote
                                    && monocode_layout::paths::same_project_path(
                                        &reminder.cwd,
                                        &location.path,
                                    )
                            })
                    })
                    .map(|reminder| ReminderDue {
                        session_id: reminder.session_id.clone(),
                        due_at: reminder.due_at,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let pending = if merged {
            local_pending || !remote_loaded
        } else {
            history.read(cx).is_pending()
        };
        let failed = history.read(cx).has_failed();
        let input = SessionListInput {
            project_sessions: &rows,
            open_sessions: &open_rows,
            busy_ids: &busy,
            approval_ids: &approvals,
            unseen_finished_ids: &unseen,
            reminders: &reminders,
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
        data.sessions_loading = if merged {
            pending && rows.is_empty()
        } else if remote_project {
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
                    machine: machines.get(&row.id).cloned(),
                    id: row.id.clone(),
                }
            })
            .collect();
        data
    }
}

/// A host session as a sidebar row of the location `cwd`.
fn host_row(host: HostSessionSummary, cwd: &str) -> SessionSummary {
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
    row
}

fn note_host_status(
    host: &HostSessionSummary,
    busy: &mut HashSet<String>,
    approvals: &mut HashSet<String>,
) {
    if host.status == HostSessionStatus::Running {
        busy.insert(host.id.clone());
    }
    if host.needs_input == Some(true) {
        approvals.insert(host.id.clone());
    }
}

/// A finished remote tab marks its host session unseen too.
fn note_unseen_hosts(
    open: &[monocode_core::Session],
    cwd: &str,
    unseen: &mut HashSet<String>,
    cx: &App,
) {
    let Some(remote) = monocode_engine::remote::RemoteGlobal::try_global(cx) else {
        return;
    };
    let connections = remote.connections.read(cx);
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
}

/// The current branch of a folder on this computer.
fn location_branch(cwd: &str, cx: &App) -> Option<String> {
    monocode_engine::projects::ProjectsGlobal::try_global(cx)
        .and_then(|projects| projects.git.read(cx).get(cwd))
        .and_then(|status| {
            status
                .read(cx)
                .index()
                .and_then(|index| index.branch.clone())
        })
}

/// The rows of every location of one project, newest first, and the
/// machine label of each row. A row listed twice keeps its first location.
pub(super) fn merge_location_rows(
    groups: Vec<(String, Vec<SessionSummary>)>,
) -> (Vec<SessionSummary>, HashMap<String, String>) {
    let mut labels = HashMap::new();
    let mut rows = Vec::new();
    for (machine, group) in groups {
        for row in group {
            if labels.contains_key(&row.id) {
                continue;
            }
            labels.insert(row.id.clone(), machine.clone());
            rows.push(row);
        }
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.updated_at));
    (rows, labels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use monocode_core::HarnessId;

    fn row(id: &str, cwd: &str, updated_at: i64) -> SessionSummary {
        let mut row = SessionSummary::new(id, cwd, HarnessId::Codex);
        row.updated_at = updated_at;
        row
    }

    #[test]
    fn merges_local_and_remote_rows_by_recency_with_their_machine() {
        let (rows, labels) = merge_location_rows(vec![
            (
                "This Mac".into(),
                vec![
                    row("local-old", "/work/app", 1),
                    row("local-new", "/work/app", 9),
                ],
            ),
            (
                "Mini".into(),
                vec![
                    row("host", "remote://mini/home/me/app", 5),
                    row("local-new", "remote://mini/home/me/app", 7),
                ],
            ),
        ]);
        let order: Vec<(&str, &str)> = rows
            .iter()
            .map(|row| (row.id.as_str(), row.cwd.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                ("local-new", "/work/app"),
                ("host", "remote://mini/home/me/app"),
                ("local-old", "/work/app"),
            ]
        );
        assert_eq!(labels["host"], "Mini");
        assert_eq!(labels["local-new"], "This Mac");
    }

    #[test]
    fn host_rows_keep_their_location() {
        let host: HostSessionSummary = serde_json::from_value(serde_json::json!({
            "projectId": "p", "revision": 1, "status": "idle", "updatedAt": 4,
            "id": "h1", "title": "Fix", "harness": "codex",
        }))
        .unwrap();
        let row = host_row(host, "remote://mini/home/me/app");
        assert_eq!(row.cwd, "remote://mini/home/me/app");
        assert_eq!(row.created_at, 4);
        assert_eq!(row.repo.as_deref(), Some("app"));
    }
}
