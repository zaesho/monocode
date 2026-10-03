//! The local shared skill library, with complete bundle imports and owned exports.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use gpui::{
    AnyElement, AnyView, App, AppContext as _, ClipboardItem, Context, Entity, FocusHandle,
    Focusable, Global, InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _,
    Render, ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    Window, canvas, div, prelude::FluentBuilder as _,
};
use gpui_base::input::InputEditorStyle;
use gpui_component::input::{InputEvent, InputState};
use monocode_app::boot::AppServices;
use monocode_core::harness::HarnessId;
use monocode_engine::submit::Submit;
use monocode_engine::submit::prefs::KvStore;
use monocode_engine::submit::skills::SkillCatalogContext;
use monocode_harness::core::provider_accounts::selected_provider_account_id;
use monocode_process::skills::{
    DiscoveredSkill, SkillDiscoveryContext, discover_skill_inventory_from,
};
use monocode_skills::{ExportState, ExportStatus, ReconcileReport, SkillEntry, SkillManager};
use monocode_ui::widgets::{button, icon_button, spinner, tooltip};
use monocode_ui::{IconName, Theme, UiStyled as _, icon, u};
use monocode_view_composer::pickers::SkillDocumentPreview;

/// CLI paths also let the manager run without booting sessions or providers.
#[derive(Clone)]
pub struct StartupOptions {
    pub data_dir: PathBuf,
    pub skills_home: Option<PathBuf>,
    pub isolated: bool,
}

impl Global for StartupOptions {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LibraryTab {
    Managed,
    Existing,
}

#[derive(Clone, PartialEq, Eq)]
enum Selection {
    Managed(String),
    Existing(String),
}

#[derive(Default)]
struct Snapshot {
    generation: u64,
    entries: Vec<SkillEntry>,
    candidates: Vec<DiscoveredSkill>,
    diagnostics: Vec<String>,
}

struct DiscoveryRequest {
    home: PathBuf,
    cwd: PathBuf,
    data_dir: Option<PathBuf>,
    kv: Option<monocode_settings::Kv>,
    isolated: bool,
}

impl DiscoveryRequest {
    fn resolve(&self) -> SkillDiscoveryContext {
        let mut context = SkillDiscoveryContext {
            home: Some(self.home.clone()),
            ..Default::default()
        };
        if let (Some(data_dir), Some(kv)) = (&self.data_dir, &self.kv) {
            let store = KvStore(kv.clone());
            let cwd = self.cwd.to_string_lossy();
            for provider in [HarnessId::Claude, HarnessId::Codex] {
                let account = selected_provider_account_id(&store, provider, Some(&cwd));
                let resolved = monocode_app::skills_runtime::resolve_context(
                    SkillCatalogContext::new(provider, cwd.as_ref()).with_account(account),
                    data_dir,
                    &self.home,
                    0,
                    self.isolated,
                );
                for (provider, path) in resolved.provider_homes {
                    context.provider_homes.insert(provider, PathBuf::from(path));
                }
            }
        }
        context
    }
}

enum Operation {
    Refresh,
    Import(PathBuf),
    Apply(String),
    Share(String, bool),
    Reconcile,
}

impl Operation {
    fn label(&self) -> &'static str {
        match self {
            Self::Refresh => "Reading skills",
            Self::Import(_) => "Importing and sharing skill",
            Self::Apply(_) => "Applying source changes",
            Self::Share(_, true) => "Sharing skill",
            Self::Share(_, false) => "Stopping managed sharing",
            Self::Reconcile => "Repairing managed copies",
        }
    }
}

struct OperationResult {
    snapshot: Snapshot,
    selected_id: Option<String>,
    notice: Option<String>,
}

pub struct SkillManagerPage {
    manager: Result<Arc<SkillManager>, String>,
    home: PathBuf,
    cwd: PathBuf,
    embedded: bool,
    beside_rail: bool,
    focus: FocusHandle,
    filter: Entity<InputState>,
    import_path: Entity<InputState>,
    query: String,
    tab: LibraryTab,
    selection: Option<Selection>,
    snapshot: Snapshot,
    busy: Option<&'static str>,
    error: Option<String>,
    notice: Option<String>,
    document: Option<Entity<SkillDocumentPreview>>,
    document_error: Option<String>,
    document_generation: u64,
    list_scroll: ScrollHandle,
    detail_scroll: ScrollHandle,
    narrow: Rc<Cell<bool>>,
    _subscriptions: Vec<Subscription>,
}

fn load_snapshot(
    manager: &SkillManager,
    cwd: &Path,
    context: &SkillDiscoveryContext,
) -> Result<Snapshot, String> {
    let managed = manager.snapshot().map_err(|error| error.to_string())?;
    let mut entries = managed.entries;
    entries.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    let inventory = discover_skill_inventory_from(cwd, context);
    Ok(Snapshot {
        generation: managed.generation,
        entries,
        candidates: inventory.candidates,
        diagnostics: inventory
            .diagnostics
            .into_iter()
            .map(|diagnostic| format!("{}: {}", diagnostic.path, diagnostic.message))
            .collect(),
    })
}

fn merge_pending_details(snapshot: &mut Snapshot, report: &ReconcileReport) {
    if report.generation != snapshot.generation {
        return;
    }
    for status in &report.statuses {
        if status.export.state != ExportState::Pending {
            continue;
        }
        if let Some(entry) = snapshot
            .entries
            .iter_mut()
            .find(|entry| entry.id == status.skill_id)
            && let Some(current) = entry.statuses.iter_mut().find(|current| {
                current.target_key == status.export.target_key && current.path == status.export.path
            })
            && current.state == ExportState::Pending
        {
            current.detail = status.export.detail.clone();
        }
    }
}

fn run_operation(
    manager: &SkillManager,
    cwd: &Path,
    context: &SkillDiscoveryContext,
    operation: Operation,
) -> Result<OperationResult, String> {
    let mut selected_id = None;
    let mut notice = None;
    let report = match operation {
        Operation::Refresh => None,
        Operation::Import(path) => {
            let imported = manager.import(path).map_err(|error| error.to_string())?;
            selected_id = Some(imported.entry.id);
            notice = Some(if imported.already_present {
                "This bundle is already in your library. Its original location is recorded.".into()
            } else {
                "Imported the complete skill bundle. Review its sharing status below.".into()
            });
            Some(imported.report)
        }
        Operation::Apply(id) => {
            let report = manager.apply(&id).map_err(|error| error.to_string())?;
            notice = Some("Applied the editable source. Review any copy conflicts below.".into());
            Some(report)
        }
        Operation::Share(id, shared) => {
            let report = manager
                .set_shared(&id, shared)
                .map_err(|error| error.to_string())?;
            notice = Some(if shared {
                "Sharing is enabled. Start a new provider session to load the updated skill.".into()
            } else {
                "Stopped sharing managed copies. Existing copies and loaded sessions may still have this skill."
                    .into()
            });
            Some(report)
        }
        Operation::Reconcile => {
            let report = manager.reconcile(&[]).map_err(|error| error.to_string())?;
            notice = Some(format!(
                "Checked managed copies. Updated {} locations. Existing edits remain protected.",
                report.changed_paths.len()
            ));
            Some(report)
        }
    };
    let mut snapshot = load_snapshot(manager, cwd, context)?;
    if let Some(report) = &report {
        merge_pending_details(&mut snapshot, report);
    }
    Ok(OperationResult {
        snapshot,
        selected_id,
        notice,
    })
}

fn selected_document_path(snapshot: &Snapshot, selection: &Option<Selection>) -> Option<PathBuf> {
    match selection {
        Some(Selection::Managed(id)) => snapshot
            .entries
            .iter()
            .find(|entry| &entry.id == id)
            .map(|entry| entry.applied_path.join("SKILL.md")),
        Some(Selection::Existing(path)) => Some(PathBuf::from(path)),
        None => None,
    }
}

impl SkillManagerPage {
    fn new(
        manager: Result<Arc<SkillManager>, String>,
        home: PathBuf,
        cwd: PathBuf,
        embedded: bool,
        beside_rail: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter skills"));
        let import_path = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Path to a folder containing SKILL.md")
        });
        let subscription = cx.subscribe(&filter, |this, input, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.query = input.read(cx).value().to_lowercase();
                cx.notify();
            }
        });
        let mut page = Self {
            error: manager.as_ref().err().cloned(),
            manager,
            home,
            cwd,
            embedded,
            beside_rail,
            focus: cx.focus_handle(),
            filter,
            import_path,
            query: String::new(),
            tab: LibraryTab::Managed,
            selection: None,
            snapshot: Snapshot::default(),
            busy: None,
            notice: None,
            document: None,
            document_error: None,
            document_generation: 0,
            list_scroll: ScrollHandle::new(),
            detail_scroll: ScrollHandle::new(),
            narrow: Rc::new(Cell::new(false)),
            _subscriptions: vec![subscription],
        };
        window.focus(&page.focus, cx);
        page.operate(Operation::Refresh, cx);
        page
    }

    fn operate(&mut self, operation: Operation, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let manager = match &self.manager {
            Ok(manager) => manager.clone(),
            Err(error) => {
                self.error = Some(error.clone());
                return;
            }
        };
        self.busy = Some(operation.label());
        self.error = None;
        self.notice = None;
        let cwd = self.cwd.clone();
        let request = self.discovery_request(cx);
        let generation_sink =
            AppServices::try_global(cx).map(|services| services.skill_generation.clone());
        cx.spawn(async move |this, cx| {
            let (result, fallback, generation) = smol::unblock(move || {
                let _lifecycle = match request
                    .data_dir
                    .as_deref()
                    .map(monocode_process::harness::lock_skill_account_lifecycle)
                    .transpose()
                {
                    Ok(guard) => guard,
                    Err(error) => return (Err(error), None, None),
                };
                let context = request.resolve();
                let result = run_operation(&manager, &cwd, &context, operation);
                let fallback = if result.is_err() {
                    load_snapshot(&manager, &cwd, &context).ok()
                } else {
                    None
                };
                let generation = match &result {
                    Ok(result) => Some(result.snapshot.generation),
                    Err(_) => fallback.as_ref().map(|snapshot| snapshot.generation),
                };
                (result, fallback, generation)
            })
            .await;
            if let Some(generation) = generation {
                if let Some(sink) = generation_sink {
                    sink.fetch_max(generation, Ordering::AcqRel);
                }
                cx.update(|cx| {
                    if let Some(submit) = Submit::try_global(cx) {
                        submit.read(cx).skills().invalidate_skills(None);
                    }
                });
            }
            this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok(result) => {
                        this.snapshot = result.snapshot;
                        this.notice = result.notice;
                        if let Some(id) = result.selected_id {
                            this.tab = LibraryTab::Managed;
                            this.selection = Some(Selection::Managed(id));
                        }
                        this.ensure_selection();
                        this.load_document(cx);
                    }
                    Err(error) => {
                        this.error = Some(error);
                        if let Some(snapshot) = fallback {
                            this.snapshot = snapshot;
                            this.ensure_selection();
                            this.load_document(cx);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn discovery_request(&self, cx: &App) -> DiscoveryRequest {
        let services = AppServices::try_global(cx);
        DiscoveryRequest {
            home: self.home.clone(),
            cwd: self.cwd.clone(),
            data_dir: services.map(|services| services.data_dir.path.clone()),
            kv: services.map(|services| services.kv.clone()),
            isolated: cx
                .try_global::<StartupOptions>()
                .is_some_and(|options| options.isolated),
        }
    }

    fn ensure_selection(&mut self) {
        let valid = match &self.selection {
            Some(Selection::Managed(id)) => {
                self.snapshot.entries.iter().any(|entry| &entry.id == id)
            }
            Some(Selection::Existing(path)) => self
                .snapshot
                .candidates
                .iter()
                .any(|candidate| &candidate.path == path),
            None => false,
        };
        if valid {
            return;
        }
        self.selection = match self.tab {
            LibraryTab::Managed => self
                .snapshot
                .entries
                .first()
                .map(|entry| Selection::Managed(entry.id.clone())),
            LibraryTab::Existing => self
                .snapshot
                .candidates
                .first()
                .map(|candidate| Selection::Existing(candidate.path.clone())),
        };
    }

    fn select(&mut self, selection: Selection, cx: &mut Context<Self>) {
        self.selection = Some(selection);
        self.detail_scroll
            .set_offset(gpui::point(gpui::px(0.), gpui::px(0.)));
        self.load_document(cx);
        cx.notify();
    }

    fn switch_tab(&mut self, tab: LibraryTab, cx: &mut Context<Self>) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        self.selection = None;
        self.list_scroll
            .set_offset(gpui::point(gpui::px(0.), gpui::px(0.)));
        self.detail_scroll
            .set_offset(gpui::point(gpui::px(0.), gpui::px(0.)));
        self.ensure_selection();
        self.load_document(cx);
        cx.notify();
    }

    fn selected_source(&self) -> Option<PathBuf> {
        selected_document_path(&self.snapshot, &self.selection)
    }

    fn load_document(&mut self, cx: &mut Context<Self>) {
        self.document_generation += 1;
        let generation = self.document_generation;
        self.document = None;
        self.document_error = None;
        let Some(path) = self.selected_source() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let text = smol::unblock(move || {
                std::fs::read_to_string(&path)
                    .map_err(|error| format!("{}: {error}", path.display()))
            })
            .await;
            this.update(cx, |this, cx| {
                if generation != this.document_generation {
                    return;
                }
                match text {
                    Ok(text) => {
                        this.document = Some(cx.new(|cx| SkillDocumentPreview::new(&text, cx)));
                    }
                    Err(error) => this.document_error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() || self.manager.is_err() {
            return;
        }
        self.busy = Some("Choosing a skill folder");
        let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Import skill folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = picked.await;
            this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            this.operate(Operation::Import(path), cx);
                        }
                    }
                    Ok(Err(error)) => this.error = Some(error.to_string()),
                    Err(error) => this.error = Some(error.to_string()),
                    _ => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn open_source(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some("Opening source folder");
        self.error = None;
        self.notice = None;
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || {
                monocode_git::fs::open_path_with_default_app(path.to_string_lossy().into_owned())
            })
            .await;
            this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok(()) => {
                        this.notice = Some(
                            "Edit SKILL.md or its supporting files in the source folder, then Apply edits."
                                .into(),
                        );
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn input(state: &Entity<InputState>, cx: &mut App) -> AnyElement {
        let theme = Theme::of(cx).clone();
        state.update(cx, |state, _| {
            state.set_editor_style(InputEditorStyle {
                foreground: theme.colors.content,
                muted_foreground: theme.content(0.40),
                background: gpui::transparent_black(),
                border: gpui::transparent_black(),
                selection: theme.accent(0.35),
                caret: theme.colors.content,
                ..Default::default()
            });
        });
        state.clone().into_any_element()
    }

    fn header(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        div()
            .flex()
            .items_center()
            .flex_none()
            .h(u(theme.metrics.title_bar_height))
            .border_b_1()
            .border_color(theme.colors.stroke)
            .pl(u(if cfg!(target_os = "macos") && !self.beside_rail {
                theme.metrics.traffic_light_inset
            } else {
                16.
            }))
            .pr(u(12.))
            .gap(u(8.))
            .text_px(theme.text.body)
            .child(div().text_color(theme.content(0.45)).child("Settings"))
            .child(div().text_color(theme.content(0.25)).child("/"))
            .child("Skills")
            .child(div().flex_1())
            .when(self.embedded, |el| {
                el.child(
                    icon_button("close-skill-manager", IconName::X)
                        .tooltip("Close skills")
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(crate::shell::OpenSettings), cx)
                        }),
                )
            })
            .into_any_element()
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let disabled = self.busy.is_some() || self.manager.is_err();
        let path_input = Self::input(&self.import_path, cx);
        let mut status = div()
            .flex()
            .items_center()
            .gap(u(8.))
            .text_px(theme.text.label);
        if let Some(label) = self.busy {
            status = status
                .child(spinner("skill-operation-spinner").color(theme.content(0.55)))
                .child(label);
        } else if let Some(error) = &self.error {
            status = status.text_color(theme.colors.danger).child(error.clone());
        } else if let Some(notice) = &self.notice {
            status = status.text_color(theme.content(0.70)).child(notice.clone());
        }
        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(u(16.))
            .px(u(if self.narrow.get() { 16. } else { 28. }))
            .pt(u(24.))
            .pb(u(18.))
            .border_b_1()
            .border_color(theme.colors.stroke)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_start()
                    .gap(u(20.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(u(6.))
                            .child(div().text_px(22.).medium().child("Skill manager"))
                            .child(
                                div().text_px(theme.text.body).text_color(theme.content(0.55)).child(
                                    "Import once and share with your local providers. Scripts and references stay together.",
                                ),
                            ),
                    )
                    .child(
                        button("rescan-skills", "Refresh")
                            .icon(IconName::RefreshCw)
                            .disabled(disabled)
                            .on_click(cx.listener(|this, _, _, cx| this.operate(Operation::Refresh, cx))),
                    )
                    .child(
                        button("reconcile-skills", "Repair sharing")
                            .icon(IconName::Share)
                            .disabled(disabled)
                            .tooltip("Restore missing managed copies. Preserve existing files and external edits.")
                            .on_click(cx.listener(|this, _, _, cx| this.operate(Operation::Reconcile, cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(u(8.))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .h(u(32.))
                            .px(u(10.))
                            .rounded(u(theme.radius.md))
                            .border_1()
                            .border_color(theme.colors.stroke)
                            .text_px(theme.text.body)
                            .child(path_input),
                    )
                    .child(
                        button("import-skill-path", "Import path")
                            .disabled(disabled)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let value = this.import_path.read(cx).value().to_string();
                                let value = value.trim();
                                let path = if value == "~" {
                                    this.home.clone()
                                } else if let Some(rest) = value.strip_prefix("~/") {
                                    this.home.join(rest)
                                } else {
                                    PathBuf::from(value)
                                };
                                if !path.is_absolute() {
                                    this.error = Some("Enter an absolute skill folder path.".into());
                                    cx.notify();
                                } else {
                                    this.operate(Operation::Import(path), cx);
                                }
                            })),
                    )
                    .child(
                        button("choose-skill-folder", "Choose folder")
                            .primary()
                            .icon(IconName::FolderPlus)
                            .disabled(disabled)
                            .on_click(cx.listener(|this, _, _, cx| this.choose_folder(cx))),
                    ),
            )
            .child(status)
            .into_any_element()
    }

    fn list_row(
        &self,
        selection: Selection,
        name: &str,
        description: &str,
        label: &str,
        path: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let selected = self.selection.as_ref() == Some(&selection);
        let hover_fill = theme.content(0.08);
        let id = match &selection {
            Selection::Managed(id) => format!("managed-{id}"),
            Selection::Existing(path) => format!("existing-{path}"),
        };
        let row = div()
            .id(SharedString::from(id))
            .flex()
            .flex_col()
            .gap(u(6.))
            .px(u(12.))
            .py(u(12.))
            .rounded(u(theme.radius.md))
            .bg(if selected {
                theme.colors.selection
            } else {
                gpui::transparent_black()
            })
            .hover(move |style| style.bg(hover_fill))
            .cursor_pointer()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(u(8.))
                    .child(
                        icon(IconName::File)
                            .size(u(14.))
                            .text_color(theme.content(0.50)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .medium()
                            .child(name.to_string()),
                    )
                    .child(
                        div()
                            .text_px(10.)
                            .text_color(if label == "Conflict" {
                                theme.colors.danger
                            } else {
                                theme.content(0.45)
                            })
                            .child(label.to_string()),
                    ),
            )
            .child(
                div()
                    .text_px(theme.text.label)
                    .text_color(theme.content(0.50))
                    .truncate()
                    .child(description.to_string()),
            )
            .when_some(path, |row, path| {
                row.child(
                    div()
                        .truncate()
                        .text_px(10.)
                        .text_color(theme.content(0.35))
                        .child(path.to_string()),
                )
                .tooltip(tooltip(path.to_string()))
            })
            .on_click(cx.listener(move |this, _, _, cx| this.select(selection.clone(), cx)));
        row.into_any_element()
    }

    fn library(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let filter = Self::input(&self.filter, cx);
        let mut rows = Vec::new();
        match self.tab {
            LibraryTab::Managed => {
                for entry in &self.snapshot.entries {
                    if !matches_query(&self.query, &entry.name, &entry.description) {
                        continue;
                    }
                    let origin = entry
                        .origins
                        .first()
                        .map(|path| path.to_string_lossy().into_owned());
                    rows.push(self.list_row(
                        Selection::Managed(entry.id.clone()),
                        &entry.name,
                        &entry.description,
                        entry_label(entry),
                        origin.as_deref(),
                        cx,
                    ));
                }
            }
            LibraryTab::Existing => {
                for candidate in &self.snapshot.candidates {
                    if !matches_query(&self.query, &candidate.name, &candidate.description) {
                        continue;
                    }
                    rows.push(self.list_row(
                        Selection::Existing(candidate.path.clone()),
                        &candidate.name,
                        &candidate.description,
                        &candidate.source,
                        Some(&candidate.path),
                        cx,
                    ));
                }
            }
        }
        let empty = rows.is_empty();
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(u(if self.narrow.get() { 240. } else { 304. }))
            .min_h_0()
            .border_r_1()
            .border_color(theme.colors.stroke)
            .child(
                div()
                    .flex()
                    .gap(u(4.))
                    .p(u(12.))
                    .child(
                        button(
                            "managed-skills-tab",
                            format!("Library {}", self.snapshot.entries.len()),
                        )
                        .ghost()
                        .selected(self.tab == LibraryTab::Managed)
                        .on_click(
                            cx.listener(|this, _, _, cx| this.switch_tab(LibraryTab::Managed, cx)),
                        ),
                    )
                    .child(
                        button(
                            "existing-skills-tab",
                            format!("Existing {}", self.snapshot.candidates.len()),
                        )
                        .ghost()
                        .selected(self.tab == LibraryTab::Existing)
                        .on_click(
                            cx.listener(|this, _, _, cx| this.switch_tab(LibraryTab::Existing, cx)),
                        ),
                    ),
            )
            .child(
                div()
                    .mx(u(12.))
                    .mb(u(8.))
                    .flex()
                    .items_center()
                    .gap(u(8.))
                    .h(u(30.))
                    .px(u(8.))
                    .rounded(u(theme.radius.md))
                    .bg(theme.content(0.04))
                    .text_px(theme.text.body)
                    .child(
                        icon(IconName::Search)
                            .size(u(14.))
                            .text_color(theme.content(0.40)),
                    )
                    .child(div().flex_1().min_w_0().child(filter)),
            )
            .child(
                div()
                    .id("skill-library-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.list_scroll)
                    .p(u(8.))
                    .children(rows)
                    .when(empty, |el| {
                        el.child(
                            div()
                                .p(u(16.))
                                .text_px(theme.text.body)
                                .text_color(theme.content(0.45))
                                .child(if self.query.is_empty() {
                                    "No skills in this list yet."
                                } else {
                                    "No matching skills."
                                }),
                        )
                    }),
            )
            .into_any_element()
    }

    fn export_row(&self, status: &ExportStatus, cx: &App) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let label = export_label(&status.state);
        let ink = if status.state == ExportState::Conflict {
            theme.colors.danger
        } else {
            theme.content(0.65)
        };
        let providers = status
            .providers
            .iter()
            .map(|provider| provider_label(provider))
            .collect::<Vec<_>>()
            .join(", ");
        div()
            .flex()
            .flex_col()
            .gap(u(5.))
            .py(u(12.))
            .border_b_1()
            .border_color(theme.content(0.07))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(u(16.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_px(theme.text.body)
                            .child(providers),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_px(theme.text.label)
                            .text_color(ink)
                            .child(label),
                    ),
            )
            .child(
                div()
                    .text_px(theme.text.label)
                    .text_color(theme.content(0.50))
                    .child(status.detail.clone()),
            )
            .when(status.state != ExportState::Unsupported, |el| {
                let path = status.path.to_string_lossy().into_owned();
                let copy_path = path.clone();
                let copy_id =
                    SharedString::from(format!("copy-export-{}-{path}", status.target_key));
                el.child(
                    div()
                        .flex()
                        .min_w_0()
                        .items_center()
                        .gap(u(8.))
                        .child(
                            div()
                                .id(SharedString::from(format!(
                                    "export-path-{}-{path}",
                                    status.target_key
                                )))
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_px(10.)
                                .text_color(theme.content(0.35))
                                .tooltip(tooltip(path.clone()))
                                .child(path),
                        )
                        .child(
                            icon_button(copy_id, IconName::Copy)
                                .size(20.)
                                .icon_size(12.)
                                .tooltip("Copy export path")
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        copy_path.clone(),
                                    ));
                                }),
                        ),
                )
            })
            .into_any_element()
    }

    fn detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let disabled = self.busy.is_some();
        let mut content = div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(u(20.))
            .max_w(u(900.))
            .w_full();
        let source = match &self.selection {
            Some(Selection::Managed(id)) => self
                .snapshot
                .entries
                .iter()
                .find(|entry| &entry.id == id)
                .map(|entry| entry.source_path.join("SKILL.md")),
            _ => self.selected_source(),
        };
        match &self.selection {
            Some(Selection::Managed(id)) => {
                if let Some(entry) = self.snapshot.entries.iter().find(|entry| &entry.id == id) {
                    let apply_id = entry.id.clone();
                    let share_id = entry.id.clone();
                    let shared = entry.shared;
                    let source_folder = entry.source_path.clone();
                    content = content
                        .child(div().flex().items_center().gap(u(12.))
                            .child(div().flex_1().min_w_0().text_px(20.).medium().child(entry.name.clone()))
                            .child(div().text_px(theme.text.label).text_color(theme.content(0.50)).child(format!("Revision {}", entry.revision))))
                        .child(div().text_px(theme.text.body).text_color(theme.content(0.65)).child(entry.description.clone()))
                        .child(div().flex().flex_wrap().gap(u(8.))
                            .child(button("apply-skill", "Apply edits").primary().icon(IconName::Check).disabled(disabled)
                                .on_click(cx.listener(move |this, _, _, cx| this.operate(Operation::Apply(apply_id.clone()), cx))))
                            .child(button("open-skill-source", "Open source folder").icon(IconName::FolderOpen).disabled(disabled)
                                .on_click(cx.listener(move |this, _, _, cx| this.open_source(source_folder.clone(), cx))))
                            .child(button("toggle-skill-sharing", if shared { "Stop sharing" } else { "Start sharing" }).disabled(disabled)
                                .on_click(cx.listener(move |this, _, _, cx| this.operate(Operation::Share(share_id.clone(), !shared), cx)))))
                        .child(div().text_px(theme.text.label).text_color(theme.content(0.50)).child(
                            if shared { "Edit the source folder, then apply the full bundle. Provider sessions may need to restart." }
                            else { "Sharing is stopped. Existing copies and sessions that already loaded this skill may still use it." },
                        ));
                    if !entry.warnings.is_empty() {
                        content = content.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(u(6.))
                                .text_px(theme.text.label)
                                .text_color(theme.colors.danger)
                                .children(
                                    entry
                                        .warnings
                                        .iter()
                                        .map(|warning| div().child(warning.clone())),
                                ),
                        );
                    }
                    content = content.child(
                        div().flex().flex_col()
                            .child(div().text_px(theme.text.body).medium().child("Provider sharing"))
                            .child(div().mt(u(6.)).text_px(theme.text.label).text_color(theme.content(0.50)).child(
                                "Exported means the complete bundle is on disk. Native loading and tool dependencies have not been checked.",
                            ))
                            .children(entry.statuses.iter().map(|status| self.export_row(status, cx))),
                    );
                }
            }
            Some(Selection::Existing(path)) => {
                if let Some(candidate) = self
                    .snapshot
                    .candidates
                    .iter()
                    .find(|candidate| &candidate.path == path)
                {
                    let source_folder = PathBuf::from(path)
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_default();
                    let import_folder = source_folder.clone();
                    let copies = self
                        .snapshot
                        .candidates
                        .iter()
                        .filter(|skill| skill.name == candidate.name)
                        .count();
                    content = content
                        .child(div().text_px(20.).medium().child(candidate.name.clone()))
                        .child(div().text_px(theme.text.body).text_color(theme.content(0.65)).child(candidate.description.clone()))
                        .child(div().text_px(theme.text.label).text_color(theme.content(0.50)).child(format!(
                            "{} skill. Found {} {} with this name.",
                            if candidate.scope == "project" { "Project" } else { "Personal" },
                            copies, if copies == 1 { "location" } else { "locations" },
                        )))
                        .child(div().flex().flex_wrap().gap(u(8.))
                            .child(button("adopt-existing-skill", "Import to library").primary().icon(IconName::Share).disabled(disabled)
                                .on_click(cx.listener(move |this, _, _, cx| this.operate(Operation::Import(import_folder.clone()), cx))))
                            .child(button("open-existing-source", "Open original folder").icon(IconName::FolderOpen).disabled(disabled)
                                .on_click(cx.listener(move |this, _, _, cx| this.open_source(source_folder.clone(), cx)))))
                        .child(div().text_px(theme.text.label).text_color(theme.content(0.50)).child(
                            "Imports copy the complete folder into an editable library source. Existing files remain in place. Same-name conflicts appear in sharing status.",
                        ));
                }
            }
            None => {
                content = content
                    .child(div().mt(u(36.)).text_px(20.).medium().child("One library for your skills"))
                    .child(div().text_px(theme.text.body).text_color(theme.content(0.55)).child(
                        "Choose a skill folder or import one from Existing. New imports share by default with supported local providers.",
                    ))
                    .child(div().text_px(theme.text.body).text_color(theme.content(0.45)).child(
                        "The manager preserves original files and protects copies edited outside MonoCode. Select a skill to inspect its source and provider status.",
                    ));
            }
        }
        if let Some(source) = source {
            let copy_path = source.to_string_lossy().into_owned();
            content = content.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(u(6.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(u(8.))
                            .child(
                                div()
                                    .text_px(theme.text.label)
                                    .text_color(theme.content(0.45))
                                    .child("Source file"),
                            )
                            .child(
                                icon_button("copy-skill-source", IconName::Copy)
                                    .size(20.)
                                    .icon_size(12.)
                                    .tooltip("Copy source path")
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy_path.clone(),
                                        ))
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_px(theme.text.label)
                            .text_color(theme.content(0.60))
                            .child(source.to_string_lossy().into_owned()),
                    ),
            );
        }
        if let Some(error) = &self.document_error {
            content = content.child(
                div()
                    .text_px(theme.text.body)
                    .text_color(theme.colors.danger)
                    .child(error.clone()),
            );
        } else if let Some(document) = &self.document {
            content = content
                .child(
                    div()
                        .pt(u(8.))
                        .border_t_1()
                        .border_color(theme.colors.stroke)
                        .text_px(theme.text.body)
                        .medium()
                        .child(if matches!(self.selection, Some(Selection::Managed(_))) {
                            "Applied skill instructions"
                        } else {
                            "Skill instructions"
                        }),
                )
                .child(document.clone());
        }
        if !self.snapshot.diagnostics.is_empty() {
            content = content.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(u(6.))
                    .text_px(theme.text.label)
                    .text_color(theme.content(0.50))
                    .child("Some skill locations could not be read.")
                    .children(
                        self.snapshot
                            .diagnostics
                            .iter()
                            .map(|message| div().child(message.clone())),
                    ),
            );
        }
        div()
            .id("skill-detail-scroll")
            .flex()
            .flex_col()
            .items_center()
            .justify_start()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.detail_scroll)
            .p(u(if self.narrow.get() { 16. } else { 28. }))
            .child(content)
            .into_any_element()
    }
}

fn matches_query(query: &str, name: &str, description: &str) -> bool {
    query.is_empty()
        || name.to_lowercase().contains(query)
        || description.to_lowercase().contains(query)
}

fn export_label(state: &ExportState) -> &'static str {
    match state {
        ExportState::Exported => "Exported",
        ExportState::Pending => "Pending",
        ExportState::Conflict => "Conflict",
        ExportState::Disabled => "Stopped",
        ExportState::Unsupported => "Manual setup",
    }
}

fn entry_label(entry: &SkillEntry) -> &'static str {
    if entry
        .statuses
        .iter()
        .any(|status| status.state == ExportState::Conflict)
    {
        "Conflict"
    } else if !entry.shared {
        "Stopped"
    } else if entry
        .statuses
        .iter()
        .any(|status| status.state == ExportState::Pending)
    {
        "Pending"
    } else {
        "Shared"
    }
}

fn provider_label(provider: &str) -> &str {
    match provider {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "cursor" => "Cursor",
        "grok" => "Grok",
        "opencode" => "OpenCode",
        "pi" => "Pi",
        "omp" => "OMP",
        "fx" => "fx",
        "hermes" => "Hermes",
        "droid" => "Factory Droid",
        "antigravity" => "Antigravity",
        other => other,
    }
}

impl Focusable for SkillManagerPage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SkillManagerPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let header = self.header(cx);
        let toolbar = self.toolbar(cx);
        let list = self.library(cx);
        let detail = self.detail(cx);
        let narrow = self.narrow.clone();
        let probe = canvas(
            move |bounds, window, _| {
                let scale = f32::from(window.rem_size()) / 16.;
                let measured_narrow = f32::from(bounds.size.width) / scale < 720.;
                if narrow.replace(measured_narrow) != measured_narrow {
                    window.refresh();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        div()
            .id("skill-manager")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .bg(theme.colors.body_glass)
            .text_color(theme.colors.content)
            .text_px(theme.text.body)
            .font_family(theme.fonts.sans.clone())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.embedded
                    && event.keystroke.key == "escape"
                    && !event.keystroke.modifiers.modified()
                {
                    cx.stop_propagation();
                    window.dispatch_action(Box::new(crate::shell::OpenSettings), cx);
                }
            }))
            .child(probe)
            .child(header)
            .child(toolbar)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(list)
                    .child(detail),
            )
    }
}

pub fn page(
    cwd: &str,
    embedded: bool,
    beside_rail: bool,
    window: &mut Window,
    cx: &mut App,
) -> Entity<SkillManagerPage> {
    let cwd = if cwd.is_empty() {
        std::env::current_dir().unwrap_or_default()
    } else {
        PathBuf::from(cwd)
    };
    let (manager, home) = if let Some(services) = AppServices::try_global(cx) {
        (services.skills.clone(), services.skill_home.clone())
    } else if let Some(options) = cx.try_global::<StartupOptions>() {
        match &options.skills_home {
            Some(home) => (
                (|| {
                    std::fs::create_dir_all(&options.data_dir)
                        .map_err(|error| format!("{}: {error}", options.data_dir.display()))?;
                    let data_dir = std::fs::canonicalize(&options.data_dir)
                        .map_err(|error| format!("{}: {error}", options.data_dir.display()))?;
                    SkillManager::open(data_dir, home)
                        .map(Arc::new)
                        .map_err(|error| error.to_string())
                })(),
                home.clone(),
            ),
            None => (
                Err("Could not resolve the skill home directory. Pass --skills-home.".into()),
                PathBuf::new(),
            ),
        }
    } else {
        (
            Err("The skill library is not configured.".into()),
            PathBuf::new(),
        )
    };
    cx.new(|cx| SkillManagerPage::new(manager, home, cwd, embedded, beside_rail, window, cx))
}

pub fn build(window: &mut Window, cx: &mut App) -> AnyView {
    page("", false, false, window, cx).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let fixture = Self(
                std::env::temp_dir().join(format!("monocode-manager-ui-{}", uuid::Uuid::new_v4())),
            );
            for name in ["home", "project"] {
                std::fs::create_dir_all(fixture.0.join(name)).unwrap();
            }
            fixture
        }

        fn manager(&self) -> SkillManager {
            SkillManager::open(self.0.join("data"), self.0.join("home")).unwrap()
        }

        fn context(&self) -> SkillDiscoveryContext {
            SkillDiscoveryContext {
                home: Some(self.0.join("home")),
                ..Default::default()
            }
        }

        fn source(&self, name: &str, body: &str) -> PathBuf {
            let source = self.0.join(name);
            std::fs::create_dir_all(&source).unwrap();
            std::fs::write(
                source.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: UI regression fixture.\n---\n\n{body}\n"),
            )
            .unwrap();
            source
        }

        fn snapshot(&self, manager: &SkillManager) -> Snapshot {
            load_snapshot(manager, &self.0.join("project"), &self.context()).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn external_export_edits_remain_conflicts_after_an_earlier_success_report() {
        let fixture = Fixture::new();
        let manager = fixture.manager();
        let imported = manager
            .import(fixture.source("external-edit", "Applied instructions."))
            .unwrap();
        let claude = imported
            .report
            .statuses
            .iter()
            .find(|status| status.export.target_key == "claude")
            .unwrap();
        assert_eq!(claude.export.state, ExportState::Exported);
        std::fs::write(claude.export.path.join("SKILL.md"), "An external edit.").unwrap();

        let mut snapshot = fixture.snapshot(&manager);
        assert_eq!(snapshot.generation, imported.report.generation);
        merge_pending_details(&mut snapshot, &imported.report);
        let current = snapshot.entries[0]
            .statuses
            .iter()
            .find(|status| status.target_key == "claude")
            .unwrap();
        assert_eq!(current.state, ExportState::Conflict);
    }

    #[test]
    fn stale_pending_details_do_not_replace_a_newer_snapshot() {
        let fixture = Fixture::new();
        let manager = fixture.manager();
        std::fs::write(fixture.0.join("home/.claude"), "Blocked export root.").unwrap();
        let mut imported = manager
            .import(fixture.source("earlier", "Earlier instructions."))
            .unwrap();
        imported
            .report
            .statuses
            .iter_mut()
            .find(|status| status.export.target_key == "claude")
            .unwrap()
            .export
            .detail = "Stale failure from an earlier operation.".into();
        manager
            .import(fixture.source("later", "Later instructions."))
            .unwrap();

        let mut snapshot = fixture.snapshot(&manager);
        assert!(snapshot.generation > imported.report.generation);
        merge_pending_details(&mut snapshot, &imported.report);
        let current = snapshot
            .entries
            .iter()
            .find(|entry| entry.id == imported.entry.id)
            .unwrap()
            .statuses
            .iter()
            .find(|status| status.target_key == "claude")
            .unwrap();
        assert_eq!(current.state, ExportState::Pending);
        assert_ne!(current.detail, "Stale failure from an earlier operation.");
    }

    #[test]
    fn preview_reads_the_applied_revision_captured_by_the_snapshot() {
        let fixture = Fixture::new();
        let manager = fixture.manager();
        let imported = manager
            .import(fixture.source("preview-revision", "Original applied instructions."))
            .unwrap();
        let captured = fixture.snapshot(&manager);
        let selected = Some(Selection::Managed(imported.entry.id.clone()));
        let captured_path = selected_document_path(&captured, &selected).unwrap();
        std::fs::write(imported.entry.source_path.join("SKILL.md"), "---\nname: preview-revision\ndescription: Changed instructions.\n---\n\nNew editable instructions.\n").unwrap();
        assert!(
            std::fs::read_to_string(&captured_path)
                .unwrap()
                .contains("Original applied instructions.")
        );

        manager.apply(&imported.entry.id).unwrap();
        let latest = fixture.snapshot(&manager);
        let latest_path = selected_document_path(&latest, &selected).unwrap();
        assert_ne!(captured_path, latest_path);
        assert!(
            std::fs::read_to_string(&captured_path)
                .unwrap()
                .contains("Original applied instructions.")
        );
        assert!(
            std::fs::read_to_string(latest_path)
                .unwrap()
                .contains("New editable instructions.")
        );
    }

    #[test]
    fn a_blocked_export_root_keeps_its_actionable_failure_in_the_ui_snapshot() {
        let fixture = Fixture(
            std::env::temp_dir().join(format!("monocode-manager-ui-{}", uuid::Uuid::new_v4())),
        );
        let home = fixture.0.join("home");
        let source = fixture.0.join("source");
        let project = fixture.0.join("project");
        for path in [&home, &source, &project] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: ui-report-test\ndescription: Test failed export reporting.\n---\n\nReview the patch.\n",
        )
        .unwrap();
        let manager = SkillManager::open(fixture.0.join("data"), &home).unwrap();
        std::fs::write(
            home.join(".claude"),
            "This file blocks the export directory.",
        )
        .unwrap();
        let context = SkillDiscoveryContext {
            home: Some(home),
            ..Default::default()
        };

        let result =
            run_operation(&manager, &project, &context, Operation::Import(source)).unwrap();
        let status = result.snapshot.entries[0]
            .statuses
            .iter()
            .find(|status| status.target_key == "claude")
            .unwrap();
        assert_eq!(status.state, ExportState::Pending);
        assert!(
            status
                .detail
                .starts_with("Cannot prepare export directory:"),
            "{}",
            status.detail
        );
    }
}
