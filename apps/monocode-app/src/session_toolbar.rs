//! The session's project, branch, and working copy controls.

use std::rc::Rc;

use gpui::AnyElement;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Subscription,
    WeakEntity, Window, div,
};
use monocode_core::{
    Session,
    session::{WorkspaceMode, session_work_cwd},
};
use monocode_engine::history::paths::looks_like_project;
use monocode_engine::{
    projects::{GitStatus, GitWatch, WatchKind, actions},
    workspace::Workspace,
};
use monocode_layout::project_return::is_blank_session;
use monocode_ui::widgets::{MenuEntry, MenuItem, context_menu, menu};
use monocode_ui::{IconName, Theme, UiStyled as _, icon, u};
use monocode_view_composer::composer::Composer;
use monocode_view_pages::widgets::project_picker::ProjectPicker;
use monocode_view_scm::ui::{
    branch_picker::{BranchPicker, BranchPickerEvent},
    worktree_picker::{WorktreePicker, WorktreePickerEvent},
};
use monocode_view_workbench::panes::workspace_picker::{
    BaseBranch, ProjectBranches, WorkspacePicker, WorkspacePickerEvent, WorkspacePickerProps,
    WorktreeEntry,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct State {
    cwd: String,
    execution_cwd: String,
    branch: Option<String>,
    busy: bool,
    removed: bool,
    draft: bool,
    mode: WorkspaceMode,
    base: Option<String>,
}

pub struct SessionToolbar {
    session_id: String,
    workspace: WeakEntity<Workspace>,
    composer: WeakEntity<Composer>,
    remote: Option<Entity<monocode_engine::remote::remote_session::RemoteSession>>,
    state: Option<State>,
    project: Option<Entity<ProjectPicker>>,
    branch: Option<Entity<BranchPicker>>,
    worktree: Option<Entity<WorktreePicker>>,
    workspace_picker: Option<Entity<WorkspacePicker>>,
    status: Option<Entity<GitStatus>>,
    watch: Option<GitWatch>,
    /// The "Run on" menu, at its click point.
    machine_menu: Option<gpui::Point<gpui::Pixels>>,
    subscriptions: Vec<Subscription>,
}

impl SessionToolbar {
    #[cfg(test)]
    pub(crate) fn workspace_picker(&self) -> Option<&Entity<WorkspacePicker>> {
        self.workspace_picker.as_ref()
    }

    pub fn new(
        session_id: String,
        workspace: WeakEntity<Workspace>,
        composer: WeakEntity<Composer>,
    ) -> Self {
        Self {
            session_id,
            workspace,
            composer,
            remote: None,
            state: None,
            project: None,
            branch: None,
            worktree: None,
            workspace_picker: None,
            status: None,
            watch: None,
            machine_menu: None,
            subscriptions: Vec::new(),
        }
    }

    pub fn set_remote_session(
        &mut self,
        remote: Option<Entity<monocode_engine::remote::remote_session::RemoteSession>>,
        cx: &mut Context<Self>,
    ) {
        if self.remote != remote {
            self.remote = remote;
            self.state = None;
            cx.notify();
        }
    }

    pub fn set_session(
        &mut self,
        session: Option<&Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = session else {
            return;
        };
        let state = State {
            cwd: session.cwd.clone(),
            execution_cwd: session_work_cwd(session).to_string(),
            branch: session.branch.clone(),
            busy: session.is_busy(),
            removed: session.worktree_removed == Some(true),
            draft: is_blank_session(Some(session))
                || session.workspace_mode.is_some() && session.worktree_cwd.is_none(),
            mode: session.workspace_mode.unwrap_or(WorkspaceMode::Current),
            base: session.worktree_base.clone(),
        };
        if self.state.as_ref() == Some(&state) {
            return;
        }
        let rebuild = self.state.as_ref().is_none_or(|old| {
            old.cwd != state.cwd
                || old.execution_cwd != state.execution_cwd
                || old.removed != state.removed
        });
        self.state = Some(state.clone());
        if rebuild {
            self.subscriptions.clear();
            self.watch = None;
            self.status = None;
            let scm = crate::adapters::scm::app_scm(cx);
            if let Some(projects) = crate::adapters::projects_data::AppProjectsData::new(cx) {
                let session_id = self.session_id.clone();
                let composer = self.composer.clone();
                self.project = Some(cx.new(|cx| {
                    ProjectPicker::new(&state.cwd, Rc::new(projects), window, cx).on_select(
                        move |cwd, window, cx| {
                            actions::on_cwd_change(&session_id, cwd, cx);
                            composer
                                .update(cx, |composer, cx| composer.focus(window, cx))
                                .ok();
                        },
                    )
                }));
            }
            let branch = cx.new(|cx| {
                BranchPicker::new(
                    scm.clone(),
                    state.execution_cwd.clone(),
                    state.branch.clone(),
                    window,
                    cx,
                )
            });
            self.subscriptions.push(cx.subscribe_in(
                &branch,
                window,
                |this, _, event, window, cx| match event {
                    BranchPickerEvent::Changed => this.branch_changed(cx),
                    BranchPickerEvent::Close => this.focus(window, cx),
                    _ => {}
                },
            ));
            self.branch = Some(branch);
            let session_id = self.session_id.clone();
            let remote = self.remote.clone();
            let select = Rc::new(move |tree, _: &mut Window, cx: &mut App| {
                select_worktree(remote.as_ref(), &session_id, tree, cx)
            });
            let worktree = cx.new(|cx| {
                WorktreePicker::new(
                    scm.clone(),
                    state.cwd.clone(),
                    state.execution_cwd.clone(),
                    false,
                    state.removed,
                    select,
                    window,
                    cx,
                )
            });
            worktree.update(cx, |picker, _| picker.set_can_manage(true));
            self.subscriptions.push(cx.subscribe_in(
                &worktree,
                window,
                |this, _, event, window, cx| match event {
                    WorktreePickerEvent::BranchChanged => this.branch_changed(cx),
                    WorktreePickerEvent::Manage => this.manage_worktrees(window, cx),
                    WorktreePickerEvent::Close => this.focus(window, cx),
                },
            ));
            self.worktree = Some(worktree);
            let picker =
                cx.new(|cx| WorkspacePicker::new(WorkspacePickerProps::default(), window, cx));
            self.subscriptions
                .push(cx.subscribe_in(&picker, window, Self::on_workspace));
            self.workspace_picker = Some(picker);
            if looks_like_project(&state.execution_cwd) {
                let status = scm.status(&state.execution_cwd, cx);
                self.watch =
                    Some(status.update(cx, |status, cx| status.watch(WatchKind::Branches, cx)));
                self.subscriptions
                    .push(cx.observe_in(&status, window, |this, _, window, cx| {
                        this.sync_picker(window, cx)
                    }));
                self.status = Some(status);
            }
        }
        if let Some(project) = &self.project {
            project.update(cx, |project, cx| project.set_cwd(&state.cwd, cx));
        }
        if let Some(branch) = &self.branch {
            branch.update(cx, |branch, cx| {
                branch.set_branch(state.branch.clone(), cx);
                branch.set_enabled(!state.busy && !state.removed, window, cx);
                branch.set_worktree(state.cwd != state.execution_cwd, cx);
            });
        }
        if let Some(worktree) = &self.worktree {
            worktree.update(cx, |picker, cx| picker.set_enabled(!state.busy, cx));
        }
        self.sync_picker(window, cx);
        cx.notify();
    }

    fn sync_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = &self.state else {
            return;
        };
        let branches = self
            .status
            .as_ref()
            .map(|status| status.read(cx).branches_state().clone());
        if let Some(remote) = &self.remote {
            let branch = branches
                .as_ref()
                .and_then(|state| state.branches.as_ref())
                .and_then(|branches| branches.current.clone());
            remote.update(cx, |remote, cx| remote.set_current_branch(branch, cx));
        }
        let props = WorkspacePickerProps {
            cwd: state.execution_cwd.clone(),
            mode: state.mode,
            base: state.base.clone(),
            enabled: !state.busy && !state.removed,
            branches: branches
                .as_ref()
                .and_then(|state| state.branches.as_ref())
                .map(|branches| ProjectBranches {
                    current: branches.current.clone(),
                    branches: branches
                        .branches
                        .iter()
                        .map(|branch| BaseBranch {
                            name: branch.name.clone(),
                            remote: branch.remote.clone(),
                        })
                        .collect(),
                }),
            settled: branches.is_some_and(|state| state.settled),
            can_select_worktree: true,
            can_open_settings: true,
            ..Default::default()
        };
        if let Some(picker) = &self.workspace_picker {
            picker.update(cx, |picker, cx| picker.set_props(props, window, cx));
        }
    }

    pub fn toggle_workspace_mode(&mut self, cx: &mut Context<Self>) {
        if self.state.as_ref().is_some_and(|state| state.draft)
            && let Some(picker) = &self.workspace_picker
        {
            picker.update(cx, |picker, cx| picker.toggle_mode(cx));
        }
    }

    fn on_workspace(
        &mut self,
        picker: &Entity<WorkspacePicker>,
        event: &WorkspacePickerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            WorkspacePickerEvent::ModeChange { mode, base } => {
                if let Some(remote) = &self.remote {
                    remote.update(cx, |remote, cx| {
                        remote.set_workspace_mode(*mode, base.clone(), cx)
                    });
                } else {
                    actions::on_workspace_mode_change(&self.session_id, *mode, base.as_deref(), cx)
                }
            }
            WorkspacePickerEvent::BaseChange(base) => {
                if let Some(remote) = &self.remote {
                    remote.update(cx, |remote, cx| remote.set_worktree_base(base.clone(), cx));
                } else {
                    actions::on_worktree_base_change(&self.session_id, base, cx)
                }
            }
            WorkspacePickerEvent::Close => self.focus(window, cx),
            WorkspacePickerEvent::OpenSettings => self.manage_worktrees(window, cx),
            WorkspacePickerEvent::LoadWorktrees { cwd } => {
                let task = actions::list_worktrees(cwd, cx);
                let picker = picker.downgrade();
                cx.spawn(async move |_, cx| {
                    let result = task.await.map(|trees| {
                        trees
                            .worktrees
                            .into_iter()
                            .map(|tree| WorktreeEntry {
                                path: tree.path,
                                branch: tree.branch,
                                head: tree.head,
                                is_main: tree.is_main,
                                missing: tree.missing,
                            })
                            .collect()
                    });
                    picker
                        .update(cx, |picker, cx| picker.set_worktrees(result, cx))
                        .ok();
                })
                .detach();
            }
            WorkspacePickerEvent::SelectWorktree(tree) => {
                let mut target =
                    monocode_engine::projects::Worktree::new(&tree.path, tree.branch.as_deref());
                target.head = tree.head.clone();
                target.is_main = tree.is_main;
                target.missing = tree.missing;
                let task = select_worktree(self.remote.as_ref(), &self.session_id, target, cx);
                let picker = picker.downgrade();
                cx.spawn_in(window, async move |_, cx| {
                    let result = task.await;
                    picker
                        .update_in(cx, |picker, window, cx| {
                            picker.worktree_selected(result, window, cx)
                        })
                        .ok();
                })
                .detach();
            }
            WorkspacePickerEvent::OpenChange(_) => {}
        }
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        self.composer
            .update(cx, |composer, cx| composer.focus(window, cx))
            .ok();
    }

    fn branch_changed(&self, cx: &mut App) {
        if let Some(remote) = &self.remote {
            remote.update(cx, |remote, cx| remote.branch_changed(cx));
        } else {
            actions::on_branch_change(&self.session_id, cx);
        }
    }

    fn manage_worktrees(&self, window: &mut Window, cx: &mut App) {
        use monocode_app::bridge::shell::{ShellPage, ShellRequest, ShellRequests};
        ShellRequests::send(ShellRequest::OpenPage(ShellPage::Settings), cx);
        window.defer(cx, |window, cx| {
            crate::pages::settings::reveal_section(
                monocode_core::settings::SettingsSectionId::Worktrees,
                window,
                cx,
            )
        });
    }

    fn browse(&mut self, cx: &mut Context<Self>) {
        let session_id = self.session_id.clone();
        let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = picked.await
                && let Some(path) = paths.first()
            {
                let path = path.to_string_lossy().to_string();
                cx.update(|cx| actions::on_cwd_change(&session_id, &path, cx));
            }
        })
        .detach();
    }
}

/// What the "Run on" picker shows for one session.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MachineChoice {
    locations: Vec<crate::machines::Location>,
    current: String,
    editable: bool,
}

impl MachineChoice {
    /// `None` when the project has one folder and no machine is paired.
    fn new(
        cwd: &str,
        locations: Vec<crate::machines::Location>,
        paired: bool,
        editable: bool,
    ) -> Option<Self> {
        if !looks_like_project(cwd) || (locations.len() < 2 && !paired) {
            return None;
        }
        let names = monocode_engine::projects::MachineNames::new();
        let current = locations
            .iter()
            .find(|location| monocode_layout::paths::same_project_path(&location.path, cwd))
            .map(|location| location.machine.clone())
            .unwrap_or_else(|| crate::machines::machine_label(cwd, &names));
        Some(Self {
            locations,
            current,
            editable,
        })
    }

    fn home(&self) -> Option<&str> {
        self.locations
            .first()
            .map(|location| location.path.as_str())
    }

    fn menu_entries(&self, cwd: &str) -> Vec<MenuEntry> {
        let mut entries: Vec<MenuEntry> = self
            .locations
            .iter()
            .map(|location| {
                let path = monocode_layout::paths::parse_remote_path(&location.path)
                    .map_or_else(|| location.path.clone(), |parts| parts.host_path);
                MenuItem::new(
                    format!("location:{}", location.path),
                    location.machine.clone(),
                )
                .description(path)
                .checked(monocode_layout::paths::same_project_path(
                    &location.path,
                    cwd,
                ))
                .into()
            })
            .collect();
        entries.push(MenuEntry::Separator);
        entries.push(MenuItem::new("add-remote", "Add on another machine…").into());
        if self.locations.iter().all(|location| location.remote) {
            entries.push(MenuItem::new("add-local", "Add folder on this computer…").into());
        }
        entries
    }
}

impl SessionToolbar {
    fn machine_choice(&self, cx: &App) -> Option<MachineChoice> {
        let state = self.state.as_ref()?;
        let locations = crate::machines::project_locations(&state.cwd, cx);
        MachineChoice::new(
            &state.cwd,
            locations,
            crate::machines::has_paired_machine(cx),
            state.draft && !state.busy,
        )
    }

    fn pick_machine(&mut self, id: &str, choice: &MachineChoice, cx: &mut Context<Self>) {
        self.machine_menu = None;
        cx.notify();
        let session_id = self.session_id.clone();
        let Some(home) = choice.home().map(str::to_string) else {
            return;
        };
        match id {
            "add-remote" => {
                use monocode_app::bridge::shell::{ShellRequest, ShellRequests};
                ShellRequests::send(
                    ShellRequest::OpenRemoteProject {
                        link_to: Some(home),
                        session_id: Some(session_id),
                    },
                    cx,
                );
            }
            "add-local" => crate::machines::add_local_location(home, Some(session_id), cx),
            _ => {
                if let Some(path) = id.strip_prefix("location:") {
                    crate::machines::move_blank_session(&session_id, path, cx);
                }
            }
        }
    }

    /// "Run on": the machine this session runs on. A menu until the first
    /// message, then a label.
    fn render_machine_picker(&self, choice: MachineChoice, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let current_remote = self
            .state
            .as_ref()
            .is_some_and(|state| monocode_layout::paths::is_remote_project_path(&state.cwd));
        let open = self.machine_menu.is_some();
        let content = theme.colors.content;
        let mut trigger = div()
            .id("session-machine-picker")
            .flex()
            .flex_none()
            .h(u(26.))
            .gap(u(6.))
            .px(u(8.))
            .items_center()
            .rounded(u(theme.radius.md))
            .text_px(theme.text.label)
            .text_color(theme.content(0.50))
            .tooltip(monocode_ui::widgets::tooltip(if choice.editable {
                "Run on"
            } else {
                "Runs on this machine"
            }))
            .child(
                icon(if current_remote {
                    IconName::Internet
                } else {
                    IconName::Monitor
                })
                .size(u(13.)),
            )
            .child(
                div()
                    .medium()
                    .text_color(theme.content(0.90))
                    .child(choice.current.clone()),
            );
        if choice.editable {
            let hover = theme.content(0.05);
            trigger = trigger
                .when(open, |el| el.bg(theme.colors.selection).text_color(content))
                .hover(move |s| s.bg(hover).text_color(content))
                .child(
                    icon(if open {
                        IconName::ChevronUp
                    } else {
                        IconName::ChevronDown
                    })
                    .size(u(12.))
                    .text_color(theme.content(0.45)),
                )
                .on_click(cx.listener(|this, event: &gpui::ClickEvent, _, cx| {
                    this.machine_menu = if this.machine_menu.is_some() {
                        None
                    } else {
                        Some(event.position())
                    };
                    cx.notify();
                }));
        }
        let mut root = div().flex_none().child(trigger);
        if let Some(position) = self.machine_menu.filter(|_| choice.editable) {
            let cwd = self
                .state
                .as_ref()
                .map(|state| state.cwd.clone())
                .unwrap_or_default();
            let pick = cx.weak_entity();
            let dismiss = pick.clone();
            let entries = choice.menu_entries(&cwd);
            root = root.child(context_menu(
                position,
                menu("session-machine-menu", entries).on_pick(move |id, _, cx| {
                    let id = id.to_string();
                    let choice = choice.clone();
                    pick.update(cx, |this, cx| this.pick_machine(&id, &choice, cx))
                        .ok();
                }),
                move |_, cx| {
                    dismiss
                        .update(cx, |this, cx| {
                            this.machine_menu = None;
                            cx.notify();
                        })
                        .ok();
                },
                cx,
            ));
        }
        root.into_any_element()
    }
}

impl Render for SessionToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = Theme::of(cx).content(0.5);
        let mut row = div().flex().min_w_0().items_center().gap(u(6.));
        if let Some(choice) = self.machine_choice(cx) {
            row = row.child(self.render_machine_picker(choice, cx));
        } else {
            self.machine_menu = None;
        }
        if self.remote.is_none()
            && let Some(project) = &self.project
        {
            row = row.child(project.clone());
        }
        if self.remote.is_none() {
            row = row.child(
                div()
                    .id("browse-session-project")
                    .cursor_pointer()
                    .p(u(3.))
                    .text_color(muted)
                    .child(icon(IconName::FolderOpen).size(u(13.)))
                    .on_click(cx.listener(|this, _, _, cx| this.browse(cx))),
            );
        }
        if self.remote.is_none() && self.state.as_ref().is_some_and(|state| !state.removed) {
            row = row.child(
                div()
                    .id("session-terminal")
                    .cursor_pointer()
                    .p(u(3.))
                    .text_color(muted)
                    .child(icon(IconName::Terminal).size(u(13.)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.workspace
                            .update(cx, |workspace, cx| {
                                workspace.new_terminal_in_session(&this.session_id, cx)
                            })
                            .ok();
                    })),
            );
        }
        if let Some(state) = &self.state {
            if state.draft && looks_like_project(&state.cwd) {
                if let Some(picker) = &self.workspace_picker {
                    row = row.child(picker.clone());
                }
                if state.mode == WorkspaceMode::Current
                    && let Some(branch) = &self.branch
                {
                    row = row.child(branch.clone());
                }
            } else if looks_like_project(&state.cwd)
                && let Some(worktree) = &self.worktree
            {
                row = row.child(worktree.clone());
            }
        }
        row
    }
}

fn select_worktree(
    remote: Option<&Entity<monocode_engine::remote::remote_session::RemoteSession>>,
    session_id: &str,
    tree: monocode_engine::projects::Worktree,
    cx: &mut App,
) -> gpui::Task<Result<(), String>> {
    if let Some(remote) = remote {
        return gpui::Task::ready(
            remote.update(cx, |remote, cx| remote.select_worktree(&tree.path, cx)),
        );
    }
    actions::on_worktree_change(session_id, tree, cx)
}

#[cfg(test)]
mod machine_tests {
    use super::*;
    use crate::machines::Location;

    fn location(path: &str, machine: &str) -> Location {
        Location {
            path: path.into(),
            machine: machine.into(),
            remote: path.starts_with("remote://"),
        }
    }

    #[test]
    fn run_on_shows_with_several_folders_or_a_paired_machine() {
        let single = vec![location("/work/app", "This Mac")];
        assert_eq!(
            MachineChoice::new("/work/app", single.clone(), false, true),
            None
        );
        let paired = MachineChoice::new("/work/app", single, true, true).unwrap();
        assert_eq!(paired.current, "This Mac");
        assert!(MachineChoice::new("~", Vec::new(), true, true).is_none());
    }

    #[test]
    fn run_on_lists_each_folder_checked_and_the_add_actions() {
        let choice = MachineChoice::new(
            "remote://mini/home/me/app",
            vec![
                location("remote://mini/home/me/app", "Mini"),
                location("remote://box/srv/app", "Atlas"),
            ],
            true,
            true,
        )
        .unwrap();
        assert_eq!(choice.current, "Mini");
        let items: Vec<(String, bool)> = choice
            .menu_entries("remote://mini/home/me/app")
            .into_iter()
            .filter_map(|entry| match entry {
                MenuEntry::Item(item) => Some((item.id.to_string(), item.checked)),
                MenuEntry::Separator => None,
            })
            .collect();
        assert_eq!(
            items,
            [
                ("location:remote://mini/home/me/app".into(), true),
                ("location:remote://box/srv/app".into(), false),
                ("add-remote".into(), false),
                ("add-local".into(), false),
            ]
        );
    }
}
