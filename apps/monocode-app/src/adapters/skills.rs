//! The shared skill manager in the Settings page's Skills section.

use gpui::{AnyView, App, Window};
use monocode_view_settings::settings::SlotContext;

/// The Settings page's Skills section.
pub fn skills_slot(ctx: &SlotContext, window: &mut Window, cx: &mut App) -> AnyView {
    crate::skill_manager::settings_slot(ctx, window, cx)
}
