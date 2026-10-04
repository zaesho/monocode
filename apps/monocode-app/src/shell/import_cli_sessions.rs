//! "Import terminal sessions": the Claude Code, Codex and Grok sessions a
//! project folder has in the terminal, imported into its history on demand.

use std::collections::HashSet;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, Context, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Task,
    WeakEntity, Window, div,
};
use monocode_app::boot::AppServices;
use monocode_core::{HarnessId, Session};
use monocode_engine::history::cli_import::{ImportReport, NewSession, import_cli_sessions};
use monocode_engine::runtime::Engine;
use monocode_engine::workspace::SessionFactory as _;
use monocode_store::cli_sessions::CliSession;
use monocode_ui::widgets::{ModalSize, button, modal};
use monocode_ui::{IconName, Theme, UiStyled as _, icon, provider_logo, u};

use super::Shell;
use crate::format::{format_relative, now_ms, provider_logo as harness_logo};

enum Listing {
    Loading,
    Failed(String),
    Ready(Vec<CliSession>),
}

pub struct ImportCliSessionsDialog {
    cwd: String,
    name: String,
    shell: WeakEntity<Shell>,
    listing: Listing,
    selected: HashSet<String>,
    /// Sessions done so far and the total, while an import runs.
    progress: Option<(usize, usize)>,
    report: Option<ImportReport>,
    _task: Option<Task<()>>,
}

fn session_key(session: &CliSession) -> String {
    format!("{}:{}", session.harness, session.provider_session_id)
}

impl ImportCliSessionsDialog {
    pub fn new(
        cwd: String,
        name: String,
        shell: WeakEntity<Shell>,
        cx: &mut Context<Self>,
    ) -> Self {
        let listing = Engine::writer(cx).cli_sessions_list(&cwd);
        let task = cx.spawn(async move |this, cx| {
            let result = listing.await;
            this.update(cx, |this, cx| {
                this.listing = match result {
                    Ok(rows) => {
                        this.selected = rows.iter().map(session_key).collect();
                        Listing::Ready(rows)
                    }
                    Err(error) => Listing::Failed(error),
                };
                cx.notify();
            })
            .ok();
        });
        Self {
            cwd,
            name,
            shell,
            listing: Listing::Loading,
            selected: HashSet::new(),
            progress: None,
            report: None,
            _task: Some(task),
        }
    }

    fn importing(&self) -> bool {
        self.progress.is_some_and(|(done, total)| done < total)
    }

    fn chosen(&self) -> Vec<CliSession> {
        match &self.listing {
            Listing::Ready(rows) => rows
                .iter()
                .filter(|row| self.selected.contains(&session_key(row)))
                .cloned()
                .collect(),
            _ => Vec::new(),
        }
    }

    fn toggle(&mut self, key: String, cx: &mut Context<Self>) {
        if self.importing() {
            return;
        }
        if !self.selected.remove(&key) {
            self.selected.insert(key);
        }
        cx.notify();
    }

    fn toggle_all(&mut self, cx: &mut Context<Self>) {
        let Listing::Ready(rows) = &self.listing else {
            return;
        };
        if self.importing() {
            return;
        }
        if self.selected.len() == rows.len() {
            self.selected.clear();
        } else {
            self.selected = rows.iter().map(session_key).collect();
        }
        cx.notify();
    }

    fn start_import(&mut self, cx: &mut Context<Self>) {
        let chosen = self.chosen();
        if chosen.is_empty() || self.importing() {
            return;
        }
        let total = chosen.len();
        self.progress = Some((0, total));
        self.report = None;
        let factory = AppServices::global(cx).factory.clone();
        let catalog = factory.env().catalog.clone();
        let new_session: Rc<NewSession<'static>> = Rc::new(
            move |harness: HarnessId, cwd: &str, model: Option<&str>| -> Session {
                factory.new_session(harness, cwd, model, None, None)
            },
        );
        let this = cx.entity().downgrade();
        let progress_target = this.clone();
        let run = import_cli_sessions(
            chosen,
            self.cwd.clone(),
            new_session,
            catalog,
            move |done, cx| {
                progress_target
                    .update(cx, |dialog, cx| {
                        dialog.progress = Some((done, total));
                        cx.notify();
                    })
                    .ok();
            },
            cx,
        );
        self._task = Some(cx.spawn(async move |_, cx| {
            let report = run.await;
            this.update(cx, |dialog, cx| {
                dialog.progress = Some((total, total));
                dialog.report = Some(report);
                if report.failed == 0 {
                    dialog.close(cx);
                } else {
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if self.importing() {
            return;
        }
        let shell = self.shell.clone();
        cx.defer(move |cx| {
            if let Some(shell) = shell.upgrade() {
                cx.update_entity(&shell, |shell, cx| shell.dismiss_project_dialog(cx));
            }
        });
    }
}

fn checkbox(id: SharedString, checked: bool, theme: &Theme) -> gpui::Stateful<gpui::Div> {
    let mut boxed = div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(u(16.))
        .rounded(u(theme.radius.sm))
        .border_1();
    boxed = if checked {
        boxed
            .border_color(theme.colors.accent)
            .bg(theme.colors.accent)
            .child(icon(IconName::Check).size(u(12.)).text_color(gpui::white()))
    } else {
        boxed.border_color(theme.content(0.20))
    };
    boxed
}

impl Render for ImportCliSessionsDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let importing = self.importing();
        let now = now_ms();
        let mut body = div()
            .flex()
            .flex_col()
            .gap(u(12.))
            .p(u(16.))
            .text_px(12.)
            .text_color(theme.content(0.85));
        let note = |text: String| -> AnyElement {
            div()
                .text_color(theme.content(0.55))
                .child(text)
                .into_any_element()
        };
        match &self.listing {
            Listing::Loading => body = body.child(note("Looking for sessions…".into())),
            Listing::Failed(error) => {
                body = body.child(div().text_color(theme.colors.danger).child(error.clone()))
            }
            Listing::Ready(rows) if rows.is_empty() => {
                body = body.child(note(
                    "No terminal sessions found for this folder that are not already in MonoCode."
                        .into(),
                ))
            }
            Listing::Ready(rows) => {
                let all = self.selected.len() == rows.len();
                body = body.child(
                    div()
                        .id("import-cli-all")
                        .flex()
                        .items_center()
                        .gap(u(8.))
                        .text_color(theme.content(0.70))
                        .child(checkbox("import-cli-all-box".into(), all, &theme))
                        .child(if rows.len() == 1 {
                            "1 session".to_string()
                        } else {
                            format!("{} sessions", rows.len())
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_all(cx))),
                );
                let mut list = div()
                    .id("import-cli-list")
                    .flex()
                    .flex_col()
                    .max_h(u(360.))
                    .overflow_y_scroll();
                for (index, row) in rows.iter().enumerate() {
                    let key = session_key(row);
                    let checked = self.selected.contains(&key);
                    let hover = theme.content(0.05);
                    let logo = row
                        .harness
                        .parse::<HarnessId>()
                        .ok()
                        .map(|harness| provider_logo(harness_logo(harness)).size(14.));
                    list = list.child(
                        div()
                            .id(("import-cli-row", index))
                            .flex()
                            .items_center()
                            .gap(u(8.))
                            .px(u(4.))
                            .py(u(6.))
                            .rounded(u(6.))
                            .hover(move |style| style.bg(hover))
                            .child(checkbox(
                                format!("import-cli-box-{index}").into(),
                                checked,
                                &theme,
                            ))
                            .children(logo)
                            .child(div().flex_1().min_w_0().truncate().child(row.title.clone()))
                            .child(
                                div()
                                    .flex_none()
                                    .text_px(11.)
                                    .text_color(theme.content(0.45))
                                    .child(format_relative(row.updated_at, now)),
                            )
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.toggle(key.clone(), cx)),
                            ),
                    );
                }
                body = body.child(list);
            }
        }
        if let Some(report) = self.report.filter(|report| report.failed > 0) {
            body = body.child(
                div()
                    .text_color(theme.colors.danger)
                    .child(if report.failed == 1 {
                        "1 session could not be imported.".to_string()
                    } else {
                        format!("{} sessions could not be imported.", report.failed)
                    }),
            );
        }
        let chosen = self.chosen().len();
        let mut footer = div().flex().items_center().justify_end().gap(u(8.));
        if let Some((done, total)) = self.progress.filter(|_| importing) {
            footer = footer.child(
                div()
                    .mr_auto()
                    .text_color(theme.content(0.55))
                    .child(format!("Importing {} of {total}…", done + 1)),
            );
        }
        footer = footer
            .child(
                button(
                    "import-cli-cancel",
                    if self.report.is_some() {
                        "Close"
                    } else {
                        "Cancel"
                    },
                )
                .ghost()
                .disabled(importing)
                .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
            )
            .child(
                button(
                    "import-cli-confirm",
                    if chosen == 1 {
                        "Import 1 session".to_string()
                    } else {
                        format!("Import {chosen} sessions")
                    },
                )
                .primary()
                .disabled(importing || chosen == 0)
                .on_click(cx.listener(|this, _, _, cx| this.start_import(cx))),
            );
        body = body.child(footer);
        let this = cx.entity().downgrade();
        modal("import-cli-sessions", "Import terminal sessions")
            .description(format!(
                "Claude Code, Codex and Grok sessions started in {} outside MonoCode. Sending a message resumes the same session, so it stays available in the CLI too.",
                self.name
            ))
            .size(ModalSize::Md)
            .on_close(move |_, cx: &mut App| {
                this.update(cx, |dialog, cx| dialog.close(cx)).ok();
            })
            .child(body)
    }
}
