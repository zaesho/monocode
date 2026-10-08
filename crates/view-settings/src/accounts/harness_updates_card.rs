//! Port of `HarnessUpdatesGroup` and `HarnessUpdateSettingsRow` in
//! SettingsView.tsx: the CLI updates card on the Providers page.
//!
//! It lists every installed CLI with a release feed. The launch notice
//! offers only harnesses shown in the model picker and is gone once
//! dismissed. Opening the page runs no CLI: the card shows the last check,
//! and Check for updates runs a new one.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, Entity, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _,
};
use monocode_ui::{IconName, Theme, UiStyled as _, icon, provider_logo, u};

use super::harness_update_notice::RowState;
use super::harness_update_store::HarnessUpdateStore;
use super::host::HarnessUpdateHost;
use super::model::{HarnessUpdate, HarnessVersionCheck, pending_harness_updates};
use crate::settings::chrome::{Reveal, group, row};
use crate::settings::controls::{Leading, secondary_button};
use crate::settings::providers::harness_logo;

/// The `harness-updates` card.
pub struct HarnessUpdatesCard {
    store: Entity<HarnessUpdateStore>,
    reveal: Reveal,
    _subscriptions: Vec<Subscription>,
}

impl HarnessUpdatesCard {
    pub fn new(host: Rc<dyn HarnessUpdateHost>, cx: &mut Context<Self>) -> Self {
        let store = HarnessUpdateStore::global(host.clone(), cx);
        let mut subscriptions = vec![cx.observe(&store, |_, _, cx| cx.notify())];
        // Another window's update leaves the listed versions stale.
        let listener = store.clone();
        if let Some(updated) = host.on_harness_updated(
            Box::new(move |_, cx| {
                listener.update(cx, |store, cx| {
                    if store.checks().is_some() {
                        store.check(false, cx).detach();
                    }
                })
            }),
            cx,
        ) {
            subscriptions.push(updated);
        }
        Self {
            store,
            reveal: Reveal::default(),
            _subscriptions: subscriptions,
        }
    }

    pub(crate) fn keep(&mut self, subscription: Subscription) {
        self._subscriptions.push(subscription);
    }

    /// The page's reveal state, so the card flashes when search finds it.
    pub fn set_reveal(&mut self, reveal: Reveal, cx: &mut Context<Self>) {
        self.reveal = reveal;
        cx.notify();
    }

    /// Update all: every harness behind its release that is not updating. A
    /// harness updated earlier in this session can fall behind again when a
    /// newer release ships, so only a running update is left out.
    pub fn pending(&self, cx: &App) -> Vec<HarnessUpdate> {
        let store = self.store.read(cx);
        pending_harness_updates(store.checks().unwrap_or_default())
            .into_iter()
            .filter(|update| store.run(update.harness) != RowState::Updating)
            .collect()
    }

    fn render_row(
        &self,
        check: &HarnessVersionCheck,
        state: RowState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let harness = check.harness();
        let title = harness.title();
        let label = div()
            .flex()
            .items_center()
            .gap(u(8.))
            .child(provider_logo(harness_logo(harness)).size(16.))
            .child(title)
            .when_some(check.versions(), |el, (installed, _)| {
                el.child(
                    div()
                        .font_family(theme.fonts.mono.clone())
                        .text_px(12.)
                        .text_color(theme.content(0.45))
                        .child(SharedString::from(installed.to_string())),
                )
            });
        let mut el = row(&self.reveal, label)
            .selector(format!("harness-update:{}", harness.as_str()))
            .description(row_description(check, &state));
        match check {
            HarnessVersionCheck::Behind {
                installed, latest, ..
            } => {
                let update = HarnessUpdate {
                    harness,
                    installed: installed.clone(),
                    latest: latest.clone(),
                };
                let updating = state == RowState::Updating;
                let failed = matches!(state, RowState::Failed(_));
                let id = if failed {
                    format!("Retry updating {title}")
                } else {
                    format!("Update {title} to {latest}")
                };
                let store = self.store.clone();
                el = el.child(
                    secondary_button(id, if failed { "Retry" } else { "Update" })
                        .leading(if updating {
                            Leading::Spinner
                        } else {
                            Leading::AccentIcon(IconName::ArrowDownCircle)
                        })
                        .disabled(updating)
                        .on_click(move |_, _, cx| {
                            let update = update.clone();
                            store.update(cx, |store, cx| store.run_update(update, cx));
                        }),
                );
            }
            HarnessVersionCheck::Current { .. } => {
                el = el.child(
                    icon(IconName::Check)
                        .size(u(16.))
                        .text_color(theme.colors.success),
                );
            }
            HarnessVersionCheck::Unknown { .. } => {}
        }
        el.into_any_element()
    }
}

/// What a row says under the harness name.
pub fn row_description(check: &HarnessVersionCheck, state: &RowState) -> String {
    match (check, state) {
        (HarnessVersionCheck::Unknown { error, .. }, _) => format!("Could not check: {error}"),
        (HarnessVersionCheck::Current { .. }, RowState::Updated(version)) => {
            format!("Updated to {version}.")
        }
        (HarnessVersionCheck::Current { .. }, _) => "Up to date.".into(),
        (HarnessVersionCheck::Behind { .. }, RowState::Updating) => "Updating…".into(),
        (HarnessVersionCheck::Behind { .. }, RowState::Failed(error)) => error.clone(),
        (HarnessVersionCheck::Behind { latest, .. }, _) => {
            format!("Version {latest} is available.")
        }
    }
}

impl Render for HarnessUpdatesCard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (checks, checking) = {
            let store = self.store.read(cx);
            (store.checks().map(<[_]>::to_vec), store.checking())
        };
        let pending = self.pending(cx);
        let mut actions = div().flex().items_center().gap(u(8.));
        if pending.len() > 1 {
            let store = self.store.clone();
            actions = actions.child(
                secondary_button("Update all", "Update all")
                    .leading(Leading::AccentIcon(IconName::ArrowDownCircle))
                    .on_click(move |_, _, cx| {
                        let pending = pending.clone();
                        store.update(cx, |store, cx| store.start(pending, cx));
                    }),
            );
        }
        let store = self.store.clone();
        actions = actions.child(
            secondary_button("Check for updates", "Check for updates")
                .leading(if checking {
                    Leading::Spinner
                } else {
                    Leading::Icon(IconName::RefreshCw)
                })
                .disabled(checking)
                .on_click(move |_, _, cx| {
                    store.update(cx, |store, cx| store.check(true, cx).detach());
                }),
        );
        let mut card = group(&self.reveal, "CLI updates")
            .id("harness-updates")
            .description("MonoCode compares each installed CLI with its newest release and updates it with the CLI's own updater. Hermes Agent and Antigravity have no release feed to compare against, so they are not listed.")
            .action(actions);
        match checks {
            None => {
                card = card.child(
                    row(
                        &self.reveal,
                        if checking {
                            "Checking installed CLIs…"
                        } else {
                            "Not checked yet"
                        },
                    )
                    .description("Check for updates runs each installed CLI to read its version, then looks up its newest release."),
                );
            }
            Some(checks) if checks.is_empty() => {
                card = card.child(
                    row(&self.reveal, "No CLIs to check")
                        .description("None of the CLIs with a release feed are installed."),
                );
            }
            Some(checks) => {
                for check in &checks {
                    let state = self.store.read(cx).run(check.harness());
                    card = card.child(self.render_row(check, state, cx));
                }
            }
        }
        card
    }
}
