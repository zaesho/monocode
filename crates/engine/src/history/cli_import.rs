//! Import sessions that Claude Code, Codex or Grok recorded in a terminal.
//!
//! `monocode_store::cli_sessions` finds the transcripts and flattens each
//! into [`CliEntry`] values. This module replays those entries through the
//! reducer, so an imported session has the same blocks a live one would,
//! and saves it with the CLI's own id as `provider_session_id`. The next
//! message then resumes that same provider session, and the CLI keeps seeing
//! the conversation too.

use std::rc::Rc;

use gpui::{App, AsyncApp, Task};
use monocode_core::block::{BlockRole, ToolPreview};
use monocode_core::reducer::{
    ReducerEnv, append_user_mut, apply_harness_events_mut, stop_streaming_mut,
};
use monocode_core::{HarnessEvent, HarnessId, ModelCatalog, Session};
use monocode_harness::providers::claude::protocol::{
    preview_from_tool, tool_kind_from_name, tool_title,
};
use monocode_harness::providers::codex::protocol::codex_command_presentation;
use monocode_store::cli_sessions::{CliSession, Entry as CliEntry, ToolEntry};
use serde_json::{Map, Value};

use crate::runtime::{Engine, SessionsEvent};

/// Builds a blank session the way "+" does: `(harness, cwd, model)`.
pub type NewSession<'a> = dyn Fn(HarnessId, &str, Option<&str>) -> Session + 'a;

/// Block ids come from uuid; timestamps come from the transcript.
struct ImportEnv {
    now: i64,
}

impl ReducerEnv for ImportEnv {
    fn new_id(&mut self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    fn now_ms(&mut self) -> i64 {
        self.now
    }
}

/// Rebuild a CLI transcript as a session in `project_cwd`.
pub fn session_from_cli_entries(
    new_session: &NewSession<'_>,
    catalog: &ModelCatalog,
    source: &CliSession,
    entries: &[CliEntry],
    project_cwd: &str,
) -> Option<Session> {
    let harness: HarnessId = source.harness.parse().ok()?;
    let mut session = new_session(harness, project_cwd, source.model.as_deref());
    session.title = source.title.clone();
    session.provider_session_id = Some(source.provider_session_id.clone());
    let mut env = ImportEnv {
        now: source.created_at,
    };
    for entry in entries {
        let at = entry_at(entry).unwrap_or(env.now);
        env.now = env.now.max(at);
        if let CliEntry::User { text, .. } = entry {
            if session.busy == Some(true) {
                stop_streaming_mut(&mut session, at);
            }
            env.now = at;
            append_user_mut(&mut env, catalog, &mut session, text, &[], None);
            continue;
        }
        let events = events_for_entry(source, entry);
        apply_harness_events_mut(&mut env, &mut session, &events);
    }
    stop_streaming_mut(&mut session, env.now.max(source.updated_at));
    session.busy = Some(false);
    session
        .blocks
        .iter()
        .any(|block| block.role == BlockRole::User)
        .then_some(session)
}

fn entry_at(entry: &CliEntry) -> Option<i64> {
    match entry {
        CliEntry::User { at, .. }
        | CliEntry::Assistant { at, .. }
        | CliEntry::Reasoning { at, .. } => *at,
        CliEntry::Tool(tool) => tool.at,
    }
}

fn events_for_entry(source: &CliSession, entry: &CliEntry) -> Vec<HarnessEvent> {
    match entry {
        CliEntry::Assistant { text, .. } => vec![
            HarnessEvent::MessageDelta { text: text.clone() },
            HarnessEvent::MessageCompleted,
        ],
        CliEntry::Reasoning { text, .. } => vec![
            HarnessEvent::ReasoningDelta { text: text.clone() },
            HarnessEvent::ReasoningCompleted,
        ],
        CliEntry::Tool(tool) => tool_events(source, tool),
        CliEntry::User { .. } => Vec::new(),
    }
}

fn tool_events(source: &CliSession, tool: &ToolEntry) -> Vec<HarnessEvent> {
    let call_id = if tool.id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        tool.id.clone()
    };
    let (title, kind, preview) = tool_presentation(source, tool);
    let paths = (!tool.paths.is_empty()).then(|| tool.paths.clone());
    vec![
        HarnessEvent::ToolStarted {
            agent_model: None,
            call_id: call_id.clone(),
            title,
            kind,
            status: None,
            background: None,
            preview,
            paths: paths.clone(),
        },
        HarnessEvent::ToolUpdated {
            agent_model: None,
            call_id,
            title: None,
            kind: None,
            status: Some(if tool.failed { "failed" } else { "completed" }.into()),
            detail: tool.output.clone(),
            preview: None,
            paths,
        },
    ]
}

/// Title, kind and preview the live adapter would have shown for this call.
fn tool_presentation(
    source: &CliSession,
    tool: &ToolEntry,
) -> (String, Option<String>, Option<ToolPreview>) {
    if source.harness == "claude" {
        let input = tool
            .input
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        return (
            tool_title(&tool.name, &input),
            Some(tool_kind_from_name(&tool.name)),
            preview_from_tool(&tool.name, &input, tool.output.as_deref()),
        );
    }
    if let Some(command) = &tool.command {
        let mut item = Map::new();
        item.insert("cwd".into(), Value::String(source.cwd.clone()));
        let shown = codex_command_presentation(&item, command);
        return (shown.title, Some("execute".into()), shown.preview);
    }
    (
        tool.title.clone().unwrap_or_else(|| tool.name.clone()),
        tool.kind.clone(),
        None,
    )
}

/// What an import run did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImportReport {
    pub imported: usize,
    /// Already in MonoCode, or no message the user typed.
    pub skipped: usize,
    pub failed: usize,
}

/// Read, rebuild and save each source in turn. Every saved session is
/// announced as `SessionsEvent::Persisted`, which puts it in the sidebar.
/// `progress` runs after each source with the count done so far.
pub fn import_cli_sessions(
    sources: Vec<CliSession>,
    project_cwd: String,
    new_session: Rc<NewSession<'static>>,
    catalog: ModelCatalog,
    progress: impl Fn(usize, &mut App) + 'static,
    cx: &mut App,
) -> Task<ImportReport> {
    cx.spawn(async move |cx: &mut AsyncApp| {
        let mut report = ImportReport::default();
        for (index, source) in sources.iter().enumerate() {
            match import_one(source, &project_cwd, new_session.as_ref(), &catalog, cx).await {
                Ok(true) => report.imported += 1,
                Ok(false) => report.skipped += 1,
                Err(error) => {
                    log::warn!(
                        "[monocode] importing {} session {} failed: {error}",
                        source.harness,
                        source.provider_session_id
                    );
                    report.failed += 1;
                }
            }
            cx.update(|cx| progress(index + 1, cx));
        }
        report
    })
}

async fn import_one(
    source: &CliSession,
    project_cwd: &str,
    new_session: &NewSession<'_>,
    catalog: &ModelCatalog,
    cx: &mut AsyncApp,
) -> Result<bool, String> {
    let read = cx.update(|cx| Engine::writer(cx).cli_session_read(&source.harness, &source.path));
    let entries = read.await?;
    let Some(session) =
        session_from_cli_entries(new_session, catalog, source, &entries, project_cwd)
    else {
        return Ok(false);
    };
    let save = cx.update(|cx| {
        Engine::writer(cx).import_session(&session, source.created_at, source.updated_at)
    });
    let Some(summary) = save.await? else {
        return Ok(false);
    };
    cx.update(|cx| {
        Engine::sessions(cx).update(cx, |_, cx| {
            cx.emit(SessionsEvent::Persisted(Box::new(summary)))
        })
    });
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use monocode_core::models::{HarnessAvailability, ModelEnv, ModelPrefs};
    use monocode_core::project_providers::ProjectProviders;

    fn build(source: &CliSession, entries: &[CliEntry]) -> Option<Session> {
        let catalog = ModelCatalog::default();
        let prefs = ModelPrefs::default();
        let availability = HarnessAvailability::default();
        let projects = ProjectProviders::default();
        let env = ModelEnv {
            catalog: &catalog,
            prefs: &prefs,
            availability: &availability,
            projects: &projects,
        };
        let new_session = |harness, cwd: &str, model: Option<&str>| {
            monocode_core::session::new_session(&env, "imported", harness, cwd, model, None, None)
        };
        session_from_cli_entries(&new_session, &catalog, source, entries, "/Users/me/app")
    }

    fn source(harness: &str) -> CliSession {
        CliSession {
            harness: harness.into(),
            provider_session_id: "11111111-1111-1111-1111-111111111111".into(),
            cwd: "/Users/me/app".into(),
            title: "Fix the login bug".into(),
            model: None,
            created_at: 1_000,
            updated_at: 9_000,
            path: "/Users/me/.claude/projects/x/1111.jsonl".into(),
        }
    }

    #[test]
    fn replays_a_claude_transcript_with_its_own_timestamps() {
        let entries = vec![
            CliEntry::User {
                text: "Fix the login bug".into(),
                at: Some(1_000),
            },
            CliEntry::Reasoning {
                text: "Check auth.rs".into(),
                at: Some(1_500),
            },
            CliEntry::Tool(ToolEntry {
                id: "toolu_1".into(),
                name: "Bash".into(),
                input: Some(serde_json::json!({ "command": "git status" })),
                output: Some("clean".into()),
                at: Some(2_000),
                ..ToolEntry::default()
            }),
            CliEntry::Assistant {
                text: "Fixed.".into(),
                at: Some(3_000),
            },
            CliEntry::User {
                text: "Thanks".into(),
                at: Some(4_000),
            },
            CliEntry::Assistant {
                text: "Anytime.".into(),
                at: Some(5_000),
            },
        ];
        let session = build(&source("claude"), &entries).expect("a session with a user turn");
        assert_eq!(session.harness, HarnessId::Claude);
        assert_eq!(
            session.provider_session_id.as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );
        assert_eq!(session.title, "Fix the login bug");
        assert_eq!(session.busy, Some(false));
        let roles: Vec<_> = session.blocks.iter().map(|block| block.role).collect();
        assert_eq!(
            roles,
            vec![
                BlockRole::User,
                BlockRole::Reasoning,
                BlockRole::Tool,
                BlockRole::Assistant,
                BlockRole::User,
                BlockRole::Assistant,
            ]
        );
        assert_eq!(session.blocks[0].started_at, Some(1_000));
        assert_eq!(session.blocks[0].duration_ms, Some(3_000));
        assert_eq!(session.blocks[4].started_at, Some(4_000));
        let tool = session.blocks[2].tool.as_ref().unwrap();
        assert_eq!(tool.call_id.as_deref(), Some("toolu_1"));
        assert_eq!(tool.status.as_deref(), Some("completed"));
        assert_eq!(tool.detail.as_deref(), Some("clean"));
        assert!(
            session
                .blocks
                .iter()
                .all(|block| block.streaming != Some(true))
        );
    }

    #[test]
    fn titles_shell_commands_from_other_providers() {
        let entries = vec![
            CliEntry::User {
                text: "Run the tests".into(),
                at: Some(1_000),
            },
            CliEntry::Tool(ToolEntry {
                id: "c1".into(),
                name: "exec_command".into(),
                command: Some("cargo test".into()),
                kind: Some("execute".into()),
                output: Some("1 failed".into()),
                failed: true,
                at: Some(2_000),
                ..ToolEntry::default()
            }),
        ];
        let session = build(&source("codex"), &entries).unwrap();
        let tool = session.blocks[1].tool.as_ref().unwrap();
        assert!(tool.title.as_deref().is_some_and(|title| !title.is_empty()));
        assert_eq!(tool.kind.as_deref(), Some("execute"));
        assert_eq!(tool.status.as_deref(), Some("failed"));
    }

    #[test]
    fn skips_transcripts_without_a_user_turn() {
        let entries = vec![CliEntry::Assistant {
            text: "Hello".into(),
            at: None,
        }];
        assert!(build(&source("grok"), &entries).is_none());
    }
}
