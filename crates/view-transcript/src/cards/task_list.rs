//! Port of src/features/sessions/ui/TaskListPreview.tsx: the agent's task
//! list with a progress pill and one state mark per item.

use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, ElementId, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    RenderOnce, SharedString, Styled as _, Transformation, Window, div, percentage, px,
};
use monocode_core::block::{TaskListItem, TaskListItemStatus};
use monocode_core::task_list::task_list_progress_label;
use monocode_ui::styled::UiStyled as _;
use monocode_ui::{IconName, Theme, icon, u};

use super::style;
use crate::motion::smooth_loop;

/// `<TaskListPreview items explanation />`.
#[derive(IntoElement)]
pub struct TaskListPreview {
    id: ElementId,
    items: Vec<TaskListItem>,
    explanation: Option<SharedString>,
    spinning: bool,
}

pub fn task_list_preview(id: impl Into<ElementId>, items: Vec<TaskListItem>) -> TaskListPreview {
    TaskListPreview {
        id: id.into(),
        items,
        explanation: None,
        spinning: true,
    }
}

impl TaskListPreview {
    pub fn explanation(mut self, explanation: Option<impl Into<SharedString>>) -> Self {
        self.explanation = explanation
            .map(Into::into)
            .filter(|text: &SharedString| !text.is_empty());
        self
    }

    /// Whether an in-progress item's loader turns. A list from a turn that
    /// is no longer running holds it still: a spinning loader redraws the
    /// whole window every frame, and a stale one would spin forever.
    pub fn spinning(mut self, spinning: bool) -> Self {
        self.spinning = spinning;
        self
    }
}

/// The text color and strike-through of an item in `status`.
pub fn item_ink(status: TaskListItemStatus, theme: &Theme) -> (Hsla, Option<Hsla>) {
    match status {
        TaskListItemStatus::Completed => (theme.content(0.4), Some(theme.content(0.25))),
        TaskListItemStatus::Cancelled => (theme.content(0.35), Some(theme.content(0.2))),
        TaskListItemStatus::InProgress => (theme.content(0.85), None),
        TaskListItemStatus::Pending => (theme.content(0.6), None),
    }
}

impl RenderOnce for TaskListPreview {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let header = div()
            .flex()
            .items_start()
            .gap(u(8.))
            .border_b(px(1.))
            .border_color(theme.colors.stroke)
            .px(u(10.))
            .py(u(8.))
            .child(
                icon(IconName::ListEnd)
                    .mt(u(2.))
                    .size(u(16.))
                    .flex_none()
                    .text_color(theme.content(0.45)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(u(12.))
                            .child(
                                div()
                                    .font_family(theme.fonts.mono.clone())
                                    .text_px(12.)
                                    .line_height(u(16.))
                                    .medium()
                                    .text_color(theme.content(0.85))
                                    .child("Tasks"),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .rounded_full()
                                    .bg(theme.content(0.07))
                                    .px(u(8.))
                                    .py(u(2.))
                                    .font_family(theme.fonts.mono.clone())
                                    .text_px(10.)
                                    .line_height(u(14.))
                                    .text_color(theme.content(0.5))
                                    .child(task_list_progress_label(&self.items)),
                            ),
                    )
                    .when_some(self.explanation.clone(), |el, explanation| {
                        el.child(
                            div()
                                .mt(u(2.))
                                .line_clamp(2)
                                .font_family(theme.fonts.sans.clone())
                                .text_px(11.5)
                                .line_height(u(16.))
                                .text_color(theme.content(0.5))
                                .child(explanation),
                        )
                    }),
            );
        let mut list = div().py(u(4.)).flex().flex_col();
        for item in &self.items {
            let (ink, strike) = item_ink(item.status, &theme);
            list = list.child(
                div()
                    .flex()
                    .items_start()
                    .gap(u(10.))
                    .min_w_0()
                    .px(u(10.))
                    .py(u(6.))
                    .child(task_state(item.status, self.spinning, &theme))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_family(theme.fonts.sans.clone())
                            .text_px(12.5)
                            .line_height(u(18.))
                            .text_color(ink)
                            .when_some(strike, strike_through)
                            .child(item.text.clone()),
                    ),
            );
        }
        div()
            .id(self.id)
            .mb(u(8.))
            .overflow_hidden()
            .rounded(u(10.))
            .border_1()
            .border_color(theme.content(0.1))
            .bg(theme.content(0.035))
            .child(header)
            .child(list)
    }
}

/// `line-through decoration-content/25`: a strike line in its own color.
pub fn strike_through<E: gpui::Styled>(mut el: E, color: Hsla) -> E {
    el.text_style().strikethrough = Some(gpui::StrikethroughStyle {
        thickness: px(1.),
        color: Some(color),
    });
    el
}

/// `TaskState`.
/// The loader spins with `motion-safe:animate-spin`; GPUI holds it still
/// when `App::reduce_motion` is set, and so does a list that is not
/// `spinning`.
fn task_state(status: TaskListItemStatus, spinning: bool, theme: &Theme) -> AnyElement {
    let frame = div()
        .mt(px(1.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(u(16.))
        .rounded_full();
    match status {
        TaskListItemStatus::Completed => frame
            .bg(style::emerald_400(0.2))
            .child(
                icon(IconName::Check)
                    .size(u(10.))
                    .text_color(style::emerald_300()),
            )
            .into_any_element(),
        TaskListItemStatus::InProgress => {
            let loader = icon(IconName::Loader)
                .size(u(16.))
                .text_color(style::sky_300());
            if !spinning {
                return frame.child(loader).into_any_element();
            }
            frame
                .child(smooth_loop(Duration::from_secs(1), move |delta| {
                    loader.with_transformation(Transformation::rotate(percentage(delta)))
                }))
                .into_any_element()
        }
        TaskListItemStatus::Cancelled => frame
            .bg(theme.content(0.08))
            .child(
                icon(IconName::Minus)
                    .size(u(10.))
                    .text_color(theme.content(0.35)),
            )
            .into_any_element(),
        TaskListItemStatus::Pending => frame
            .border_1()
            .border_color(theme.content(0.25))
            .bg(theme.content(0.02))
            .into_any_element(),
    }
}
