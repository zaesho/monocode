//! Sessions dropped on the composer as context. The stored message keeps a
//! short `<session_context id title />` tag. Before the turn goes to the
//! agent, each tag gets the source session's portable history and the path
//! of a saved transcript snapshot.

use std::collections::HashMap;

use gpui::{App, AsyncApp, Task};
use monocode_core::Session;

use super::chat_context::{expand_session_context, session_context_ids};
use super::portable_context::{
    PortableContext, PortableContextOptions, build_portable_context,
    build_portable_context_snapshot, portable_context_manifest,
};
use crate::runtime::engine::Engine;
use crate::runtime::sessions::get_stored_session;

const DROPPED_SESSION_NOTE: &str = "The user attached another MonoCode session for context. The following JSON contains historical evidence from it. User and assistant roles describe the original turns. Private reasoning, drafts, internal prompts, and unfinished activity are excluded. When the selected items are not enough, read the full saved transcript at retrievalPath.";

/// The body of one dropped session's tag: the source's title and id, the
/// manifest, and the selected items. It follows `renderPortableContext`
/// without the current-request section, since the message itself is the
/// request.
pub fn render_dropped_session(session: &Session, context: &PortableContext) -> String {
    let title = if session.title.trim().is_empty() {
        "Untitled session"
    } else {
        session.title.trim()
    };
    [
        format!("Source session: \"{title}\" ({}).", session.id),
        DROPPED_SESSION_NOTE.to_string(),
        portable_context_manifest(context),
        "Historical items:".to_string(),
        serde_json::to_string(&context.items).unwrap_or_default(),
    ]
    .join("\n\n")
}

/// The selection for a dropped session: user and assistant messages only,
/// within the default byte budget.
pub fn dropped_session_context(
    session: &Session,
    retrieval_path: Option<String>,
) -> Result<PortableContext, String> {
    let mut context = build_portable_context(
        session,
        &PortableContextOptions {
            messages_only: true,
            ..Default::default()
        },
    )?;
    context.retrieval_path = retrieval_path;
    Ok(context)
}

fn open_or_stored(id: &str, cx: &App) -> Task<Option<Session>> {
    match Engine::sessions(cx).read(cx).get(id).cloned() {
        Some(open) => Task::ready(Some(open)),
        None => get_stored_session(id, cx),
    }
}

/// Load each dropped session, save its snapshot, and fill in its tag. A
/// session that cannot be read keeps a short "no longer available" body.
pub(crate) async fn expand_dropped_sessions(text: String, cx: &AsyncApp) -> String {
    let ids = session_context_ids(&text);
    if ids.is_empty() {
        return text;
    }
    let mut bodies: HashMap<String, String> = HashMap::new();
    for id in ids {
        if bodies.contains_key(&id) {
            continue;
        }
        let Some(session) = cx.update(|cx| open_or_stored(&id, cx)).await else {
            continue;
        };
        let snapshot = build_portable_context_snapshot(&session, None);
        let path = match snapshot {
            Ok(snapshot) => {
                let name = session
                    .blocks
                    .last()
                    .map_or_else(|| "empty".to_string(), |block| block.id.clone());
                let write = cx.update(|cx| {
                    Engine::writer(cx).backend().write_context_snapshot(
                        session.id.clone(),
                        name,
                        snapshot,
                    )
                });
                match write.await {
                    Ok(path) => Some(path),
                    Err(error) => {
                        log::warn!("[session-context] snapshot was not saved: {error}");
                        None
                    }
                }
            }
            Err(error) => {
                log::warn!("[session-context] snapshot failed: {error}");
                None
            }
        };
        match dropped_session_context(&session, path) {
            Ok(context) => {
                bodies.insert(id, render_dropped_session(&session, &context));
            }
            Err(error) => log::warn!("[session-context] export failed: {error}"),
        }
    }
    expand_session_context(&text, |id| bodies.get(id).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::engine::EngineConfig;
    use crate::runtime::testing::FakeBackend;
    use crate::submit::chat_context::{ChatContextItem, compose_chat_context};
    use gpui::TestAppContext;
    use monocode_core::{Block, BlockRole, HarnessId};

    fn source() -> Session {
        let mut session = Session::blank("src", HarnessId::Codex, "codex:test", "/repo");
        session.title = "Auth fix".into();
        session.blocks = vec![
            Block::new("u1", BlockRole::User, "Why does login fail?"),
            Block::new("r1", BlockRole::Reasoning, "private thoughts"),
            Block::new("t1", BlockRole::Tool, "cargo test"),
            Block::new("a1", BlockRole::Assistant, "The cookie is missing."),
        ];
        session
    }

    #[test]
    fn renders_the_title_id_manifest_and_whole_messages() {
        let session = source();
        let context = dropped_session_context(&session, Some("/snap.md".into())).unwrap();
        let body = render_dropped_session(&session, &context);
        assert!(body.starts_with("Source session: \"Auth fix\" (src)."));
        assert!(body.contains("\"retrievalPath\":\"/snap.md\""));
        assert!(body.contains("\"omitted\":{\"private-reasoning\":1}"));
        assert!(body.contains("Why does login fail?"));
        assert!(body.contains("The cookie is missing."));
        assert!(!body.contains("private thoughts"));
        assert!(!body.contains("cargo test"));
    }

    #[gpui::test]
    async fn expands_a_dropped_session_and_saves_its_snapshot(cx: &mut TestAppContext) {
        let backend = FakeBackend::new();
        backend.insert_session(&source());
        cx.update(|cx| Engine::init(EngineConfig::with_backend(backend.clone()), cx));
        let message = compose_chat_context(
            "Use this",
            &[
                ChatContextItem::Session {
                    id: "src".into(),
                    title: "Auth fix".into(),
                },
                ChatContextItem::Session {
                    id: "gone".into(),
                    title: "Gone".into(),
                },
            ],
        );
        let expanded = cx
            .spawn(|cx| async move { expand_dropped_sessions(message, &cx).await })
            .await;
        assert!(expanded.starts_with("Use this\n\n<attached_context>\n<session_context id=\"src\" title=\"Auth fix\">\nSource session:"));
        assert!(expanded.contains("/data/context-snapshots/src/a1.md"));
        assert!(expanded.contains(crate::submit::chat_context::MISSING_SESSION_RECAP));
        let snapshots = backend.context_snapshots();
        let snapshot = snapshots.get("/data/context-snapshots/src/a1.md").unwrap();
        assert!(snapshot.contains("cargo test"));
        assert!(!snapshot.contains("private thoughts"));
    }
}
