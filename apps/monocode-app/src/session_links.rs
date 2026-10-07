//! Session titles for dropped and linked sessions. A session that is not
//! open in this window is read from the store.

use gpui::{App, Task};
use monocode_engine::runtime::Engine;
use monocode_engine::runtime::sessions::get_stored_session;

/// The session's title, or an empty string when it has none or is gone.
pub fn session_title(id: &str, cx: &App) -> Task<String> {
    if let Some(open) = Engine::sessions(cx).read(cx).get(id) {
        return Task::ready(open.title.clone());
    }
    let stored = get_stored_session(id, cx);
    cx.spawn(async move |_| {
        stored
            .await
            .map(|session| session.title)
            .unwrap_or_default()
    })
}
