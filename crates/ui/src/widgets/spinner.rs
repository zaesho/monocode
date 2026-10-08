//! `TerminalSpinner`: a braille dot spinner that steps every 80ms.
//!
//! It redraws only when the frame changes, through
//! [`crate::ticker::loading_step`]. A repeating `with_animation` would
//! re-render the whole window on every display refresh while a session runs.
//! Like the React spinner, it keeps turning with reduced motion.

use std::time::Duration;

use gpui::{
    App, ElementId, Hsla, IntoElement, ParentElement as _, RenderOnce, Styled as _, Window, div,
};

use crate::styled::UiStyled as _;
use crate::ticker::loading_step;
use crate::{Theme, u};

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const FRAME: Duration = Duration::from_millis(80);

#[derive(IntoElement)]
pub struct Spinner {
    /// Kept so callers keep naming their spinners. The step clock is shared,
    /// so every spinner shows the same frame.
    #[allow(dead_code)]
    id: ElementId,
    color: Option<Hsla>,
    size: f32,
}

pub fn spinner(id: impl Into<ElementId>) -> Spinner {
    Spinner {
        id: id.into(),
        color: None,
        size: 11.,
    }
}

impl Spinner {
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }

    /// Font size in CSS px. Default 11.
    pub fn size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }
}

impl RenderOnce for Spinner {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let index = loading_step(FRAME * FRAMES.len() as u32, FRAMES.len() as u32, window, cx);
        let theme = Theme::of(cx);
        div()
            .flex()
            .flex_none()
            .justify_center()
            .w(u(12.))
            .text_px(self.size)
            .leading(theme.leading.none)
            .text_color(self.color.unwrap_or(theme.colors.accent))
            .child(FRAMES[index as usize])
    }
}
