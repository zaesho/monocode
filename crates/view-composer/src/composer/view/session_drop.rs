//! A session card from the sidebar dropped on the composer. The composer
//! shows "Add to context" while the card is over it, then asks what to do:
//! add the session to this message's context, or link the two sessions.

use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div,
};
use monocode_ui::drag::PaneDragSource;
use monocode_ui::{IconName, Theme, UiStyled as _, icon, u};

use super::super::model::chat_context::{ChatContextItem, add_chat_context};
use super::{Composer, ComposerEvent};

impl Composer {
    /// Whether the composer takes this drag: another session's card.
    pub fn accepts_session_drag(&self, source: &PaneDragSource) -> bool {
        match source {
            PaneDragSource::Session(id) => {
                self.props.enabled
                    && !self.props.disabled
                    && self.props.session_id.as_deref() != Some(id.as_str())
            }
            PaneDragSource::WorkspaceTab(_) => false,
        }
    }

    /// Shows or hides the "Add to context" overlay.
    pub fn set_session_drag(&mut self, over: bool, cx: &mut Context<Self>) {
        if over {
            // The pane tree drew its split hint for this move first; the
            // owner hides it while the composer holds the drag.
            cx.emit(ComposerEvent::SessionDragOver);
        }
        if over != self.session_drag {
            self.session_drag = over;
            cx.notify();
        }
    }

    /// A drop on the composer. Another session's card asks what to do with
    /// it. Anything else goes back to the owner, which passes it to the pane
    /// tree, so a tab or a session dropped on its own pane still splits or
    /// does nothing as before.
    pub fn on_pane_drop(&mut self, source: &PaneDragSource, cx: &mut Context<Self>) {
        self.session_drag = false;
        if self.accepts_session_drag(source) {
            self.session_drop = Some(source.id().to_string());
            cx.emit(ComposerEvent::SessionDropped);
        } else {
            cx.emit(ComposerEvent::ForwardDrop(source.clone()));
        }
        cx.notify();
    }

    /// The dropped session waiting for a choice.
    pub fn pending_session_drop(&self) -> Option<&str> {
        self.session_drop.as_deref()
    }

    /// "Add to context": the owner looks up the title and calls
    /// [`Composer::add_context_item`].
    pub fn choose_add_session_context(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.session_drop.take() {
            cx.emit(ComposerEvent::AddSessionContext(id));
            cx.notify();
        }
    }

    /// "Link sessions".
    pub fn choose_link_session(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.session_drop.take() {
            cx.emit(ComposerEvent::LinkSession(id));
            cx.notify();
        }
    }

    pub fn dismiss_session_drop(&mut self, cx: &mut Context<Self>) {
        if self.session_drop.take().is_some() {
            cx.notify();
        }
    }

    /// Adds one context chip unless the same one is already attached.
    pub fn add_context_item(&mut self, item: ChatContextItem, cx: &mut Context<Self>) {
        let items = add_chat_context(&self.context_items, item);
        self.set_context_items(items, cx);
    }

    pub(crate) fn render_session_drag_overlay(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.session_drag {
            return None;
        }
        let theme = Theme::of(cx);
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .gap(u(6.))
                .rounded(u(theme.radius.lg))
                .bg(theme.accent(0.08))
                .text_px(12.)
                .text_color(theme.content(0.70))
                .child(icon(IconName::Chatting).size(u(14.)))
                .child("Add to context")
                .into_any_element(),
        )
    }

    /// The choice after a drop: two actions and a dismiss button.
    pub(crate) fn render_session_drop_choice(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.session_drop.as_ref()?;
        let theme = Theme::of(cx).clone();
        let action = |id: &'static str, label: &'static str, glyph: IconName| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(u(6.))
                .px(u(10.))
                .py(u(4.))
                .rounded(u(theme.radius.md))
                .border_1()
                .border_color(theme.content(0.12))
                .text_px(12.)
                .text_color(theme.content(0.85))
                .cursor_pointer()
                .hover(|style| style.bg(theme.content(0.06)))
                .child(icon(glyph).size(u(14.)).text_color(theme.content(0.55)))
                .child(label)
        };
        Some(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(u(6.))
                .px(u(12.))
                .pt(u(8.))
                .child(
                    div()
                        .text_px(12.)
                        .text_color(theme.content(0.55))
                        .child("Dropped session:"),
                )
                .child(
                    action("session-drop-context", "Add to context", IconName::Chatting).on_click(
                        cx.listener(|this, _, _, cx| this.choose_add_session_context(cx)),
                    ),
                )
                .child(
                    action(
                        "session-drop-link",
                        "Link sessions",
                        IconName::MessageMultiple,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.choose_link_session(cx))),
                )
                .child(
                    div()
                        .id("session-drop-dismiss")
                        .p(u(4.))
                        .rounded(u(theme.radius.md))
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.content(0.06)))
                        .child(
                            icon(IconName::X)
                                .size(u(12.))
                                .text_color(theme.content(0.45)),
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.dismiss_session_drop(cx))),
                )
                .into_any_element(),
        )
    }
}
