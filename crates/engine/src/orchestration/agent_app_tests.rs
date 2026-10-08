//! Port of src/features/agent-app/model/agentApp.test.ts.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use gpui::{App, Task, TestAppContext};
use monocode_core::models::{ModelSetting, ModelSettingChoice, ModelSettingKind};
use monocode_core::{AgentModel, Block, BlockRole, HarnessId, ModelCatalog, RuntimeMode, Session};
use monocode_layout::SplitDir;
use monocode_settings::Kv;
use monocode_store::notes::{Note, NoteUpsert};
use serde_json::{Value, json};

use super::agent_app::{
    AgentAppHost, AppLaunch, AppSessionListing, AppSessionPlacement, DraftResult, LinkedPeer,
    LinkedSendResult, SendResult, handle_agent_app, note_preview,
};
use super::testing::finish;
use crate::history::session_folders::{SessionFolder, load_session_folders, save_session_folders};
use crate::projects::backend::{Worktree, Worktrees};
use crate::runtime::session_links::LinkSpend;

const CWD: &str = "/tmp/project";

fn note() -> Note {
    Note {
        id: "n1".into(),
        slug: "plan".into(),
        title: "Plan".into(),
        body: "First paragraph.\n\nSecond paragraph.\n\nThird paragraph should stay out of list."
            .into(),
        tags: vec!["work".into()],
        source_session_id: None,
        source_cwd: None,
        created_at: 1,
        updated_at: 2,
    }
}

fn feature_worktree() -> Worktree {
    Worktree {
        head: "abc123".into(),
        dirty: Some(false),
        unpushed: Some(0),
        ..Worktree::new("/tmp/project-worktrees/feature", Some("feature"))
    }
}

fn catalog() -> ModelCatalog {
    let mut catalog = ModelCatalog::new();
    let mut model = AgentModel::new("codex:test", HarnessId::Codex, "Test model");
    model.settings = Some(vec![ModelSetting {
        id: "effort".into(),
        label: "Effort".into(),
        kind: ModelSettingKind::Select,
        value: "medium".into(),
        options: vec![
            ModelSettingChoice {
                value: "medium".into(),
                label: "Medium".into(),
            },
            ModelSettingChoice {
                value: "high".into(),
                label: "High".into(),
            },
        ],
        description: None,
    }]);
    catalog.set_harness_models(HarnessId::Codex, vec![model]);
    catalog
}

#[derive(Default)]
struct FakeAppHost {
    starts: RefCell<Vec<(AppLaunch, String, Option<AppSessionPlacement>)>>,
    listed: RefCell<Vec<AppSessionListing>>,
    sends: RefCell<Vec<(String, String, String)>>,
    drafts: RefCell<Vec<(String, String, String)>>,
    worktrees: RefCell<Vec<String>>,
    worktrees_once: RefCell<VecDeque<Worktrees>>,
    created_worktrees: RefCell<Vec<(String, String, String, bool)>>,
    notes: RefCell<Vec<Note>>,
    saved_notes: RefCell<Vec<NoteUpsert>>,
    kv: Option<Kv>,
    open_sessions: bool,
    review_opened: bool,
    /// Linked peer ids and the agent messages each link has carried.
    links: RefCell<Vec<(String, u32)>>,
    peer_busy: bool,
    linked_sends: RefCell<Vec<(String, String, String)>>,
    fail_linked_send: bool,
}

fn other_listing() -> AppSessionListing {
    AppSessionListing {
        id: "other".into(),
        title: "Other".into(),
        harness: HarnessId::Codex,
        model: "codex:test".into(),
        busy: false,
        has_draft: false,
    }
}

impl AgentAppHost for FakeAppHost {
    fn start(
        &self,
        launch: AppLaunch,
        id: &str,
        placement: Option<AppSessionPlacement>,
        _cx: &mut App,
    ) -> Task<Result<(), String>> {
        self.starts
            .borrow_mut()
            .push((launch, id.to_string(), placement));
        Task::ready(Ok(()))
    }

    fn sessions(&self, _cwd: &str, _cx: &mut App) -> Task<Result<Vec<AppSessionListing>, String>> {
        Task::ready(Ok(self.listed.borrow().clone()))
    }

    fn session(&self, id: &str, _cx: &mut App) -> Task<Result<Option<Session>, String>> {
        Task::ready(Ok(
            (id == "other").then(|| Session::blank(id, HarnessId::Codex, "codex:test", CWD))
        ))
    }

    fn send(
        &self,
        id: &str,
        prompt: &str,
        request_id: &str,
        _cx: &mut App,
    ) -> Task<Result<SendResult, String>> {
        self.sends
            .borrow_mut()
            .push((id.into(), prompt.into(), request_id.into()));
        Task::ready(Ok(SendResult {
            already_submitted: false,
        }))
    }

    fn draft(
        &self,
        id: &str,
        prompt: &str,
        request_id: &str,
        _cx: &mut App,
    ) -> Task<Result<DraftResult, String>> {
        self.drafts
            .borrow_mut()
            .push((id.into(), prompt.into(), request_id.into()));
        Task::ready(Ok(DraftResult {
            already_saved: false,
            draft: true,
        }))
    }

    fn worktrees(&self, cwd: &str, _cx: &mut App) -> Task<Result<Worktrees, String>> {
        self.worktrees.borrow_mut().push(cwd.into());
        if let Some(once) = self.worktrees_once.borrow_mut().pop_front() {
            return Task::ready(Ok(once));
        }
        Task::ready(Ok(Worktrees {
            worktrees: vec![
                feature_worktree(),
                Worktree {
                    path: CWD.into(),
                    is_main: true,
                    branch: Some("main".into()),
                    ..feature_worktree()
                },
            ],
            default_root: "/tmp/project-worktrees".into(),
        }))
    }

    fn create_worktree(
        &self,
        cwd: &str,
        branch: &str,
        base: &str,
        existing: bool,
        _cx: &mut App,
    ) -> Task<Result<Worktree, String>> {
        self.created_worktrees.borrow_mut().push((
            cwd.into(),
            branch.into(),
            base.into(),
            existing,
        ));
        Task::ready(Ok(feature_worktree()))
    }

    fn notes(&self, _cx: &mut App) -> Task<Result<Vec<Note>, String>> {
        Task::ready(Ok(self.notes.borrow().clone()))
    }

    fn note(&self, id: &str, _cx: &mut App) -> Task<Result<Option<Note>, String>> {
        Task::ready(Ok(self.notes.borrow().iter().find(|n| n.id == id).cloned()))
    }

    fn save_note(&self, upsert: NoteUpsert, _cx: &mut App) -> Task<Result<Note, String>> {
        self.saved_notes.borrow_mut().push(upsert.clone());
        let saved = Note {
            id: upsert.id.clone(),
            title: upsert.title,
            body: upsert.body,
            tags: upsert.tags,
            source_session_id: upsert.source_session_id,
            source_cwd: upsert.source_cwd,
            ..note()
        };
        let mut notes = self.notes.borrow_mut();
        notes.retain(|n| n.id != saved.id);
        notes.push(saved.clone());
        Task::ready(Ok(saved))
    }

    fn catalog(&self, _cx: &App) -> ModelCatalog {
        catalog()
    }

    fn is_harness_available(&self, harness: HarnessId, _cx: &App) -> bool {
        harness == HarnessId::Codex
    }

    fn preferred_model_id(&self, harness: HarnessId, _cx: &App) -> String {
        catalog().default_model_id(harness)
    }

    fn kv(&self) -> Kv {
        self.kv.clone().unwrap_or_else(Kv::in_memory)
    }

    fn agent_sessions_enabled(&self, _cx: &App) -> bool {
        self.open_sessions
    }

    fn agent_sessions_review(&self, _cx: &App) -> bool {
        self.review_opened
    }

    fn linked_peers(&self, _id: &str, _cx: &App) -> Vec<LinkedPeer> {
        self.links
            .borrow()
            .iter()
            .map(|(id, sent)| LinkedPeer {
                id: id.clone(),
                messages_left: 5 - sent,
            })
            .collect()
    }

    fn peer_session(&self, id: &str, _cx: &mut App) -> Task<Result<Option<Session>, String>> {
        let linked = self.links.borrow().iter().any(|(peer, _)| peer == id);
        Task::ready(Ok(linked.then(|| {
            let mut session = Session::blank(id, HarnessId::Codex, "codex:test", "/elsewhere");
            session.title = format!("Peer {id}");
            session.blocks = vec![
                Block::new("u1", BlockRole::User, "Build the API"),
                Block::new("a1", BlockRole::Assistant, "The API is done."),
            ];
            session
        })))
    }

    fn spend_link_message(
        &self,
        _from: &str,
        to: &str,
        _cx: &mut App,
    ) -> Result<LinkSpend, String> {
        let mut links = self.links.borrow_mut();
        let entry = links
            .iter_mut()
            .find(|(peer, _)| peer == to)
            .ok_or("not linked")?;
        if entry.1 >= 5 {
            return Err("This link already carried 5 agent messages.".into());
        }
        entry.1 += 1;
        Ok(LinkSpend {
            left: 5 - entry.1,
            epoch: 0,
        })
    }

    fn refund_link_message(&self, _from: &str, to: &str, _epoch: u64, _cx: &mut App) {
        if let Some(entry) = self
            .links
            .borrow_mut()
            .iter_mut()
            .find(|(peer, _)| peer == to)
        {
            entry.1 = entry.1.saturating_sub(1);
        }
    }

    fn send_linked(
        &self,
        id: &str,
        text: &str,
        request_id: &str,
        _cx: &mut App,
    ) -> Task<Result<LinkedSendResult, String>> {
        if self.fail_linked_send {
            return Task::ready(Err("The linked session is unavailable".into()));
        }
        self.linked_sends
            .borrow_mut()
            .push((id.into(), text.into(), request_id.into()));
        Task::ready(Ok(LinkedSendResult {
            queued: self.peer_busy,
            already_submitted: false,
        }))
    }
}

struct F {
    source: Session,
    host: Rc<FakeAppHost>,
}

fn operator_turn() -> Block {
    Block {
        monocode: Some(true),
        ..Block::new("op", BlockRole::User, "list my sessions")
    }
}

fn fixture() -> F {
    let mut source = Session::blank("lead", HarnessId::Codex, "codex:test", CWD);
    source.model_settings = [("effort".to_string(), "medium".to_string())].into();
    source.blocks = vec![operator_turn()];
    let host = FakeAppHost {
        listed: RefCell::new(vec![other_listing()]),
        notes: RefCell::new(vec![note()]),
        kv: Some(Kv::in_memory()),
        ..Default::default()
    };
    F {
        source,
        host: Rc::new(host),
    }
}

impl F {
    fn call(
        &self,
        cx: &mut TestAppContext,
        request_id: &str,
        action: &str,
        input: Value,
    ) -> Result<Value, String> {
        let (source, host) = (self.source.clone(), self.host.clone());
        let (request_id, action) = (request_id.to_string(), action.to_string());
        let input = input.as_object().cloned().unwrap_or_default();
        let task = cx.spawn(|mut cx| async move {
            handle_agent_app(
                &source,
                &request_id,
                &action,
                &input,
                host.as_ref(),
                &mut cx,
            )
            .await
        });
        finish(cx, task)
    }
}

#[gpui::test]
fn reads_a_listed_project_session_in_bounded_pages(cx: &mut TestAppContext) {
    let f = fixture();
    let result = f
        .call(
            cx,
            "read-1",
            "sessions.read",
            json!({ "sessionId": "other" }),
        )
        .unwrap();
    assert_eq!(result["sessionId"], "other");
    assert_eq!(result["turns"], json!([]));
    assert!(
        f.call(
            cx,
            "read-2",
            "sessions.read",
            json!({ "sessionId": "missing" })
        )
        .unwrap_err()
        .contains("not found in this project")
    );
}

#[gpui::test]
fn sends_a_follow_up_only_to_a_listed_idle_session(cx: &mut TestAppContext) {
    let f = fixture();
    let result = f
        .call(
            cx,
            "send-1",
            "sessions.send",
            json!({ "sessionId": "other", "prompt": "Continue the review" }),
        )
        .unwrap();
    assert_eq!(result["sessionId"], "other");
    assert_eq!(result["submitted"], true);
    assert_eq!(
        f.host.sends.borrow()[0],
        (
            "other".into(),
            "Continue the review".into(),
            "app-lead-send-1".into()
        )
    );
    assert!(
        f.call(
            cx,
            "send-2",
            "sessions.send",
            json!({ "sessionId": "lead", "prompt": "loop" })
        )
        .unwrap_err()
        .contains("current conversation")
    );
    assert!(
        f.call(
            cx,
            "send-3",
            "sessions.send",
            json!({ "sessionId": "missing", "prompt": "hello" })
        )
        .unwrap_err()
        .contains("not found in this project")
    );
    assert_eq!(f.host.sends.borrow().len(), 1);
}

#[gpui::test]
fn saves_an_unsent_draft_in_another_listed_project_session(cx: &mut TestAppContext) {
    let f = fixture();
    let result = f
        .call(
            cx,
            "draft-1",
            "sessions.draft",
            json!({ "sessionId": "other", "prompt": "Review this idea later" }),
        )
        .unwrap();
    assert_eq!(result["saved"], true);
    assert_eq!(result["draft"], true);
    assert_eq!(
        f.host.drafts.borrow()[0],
        (
            "other".into(),
            "Review this idea later".into(),
            "app-lead-draft-1".into()
        )
    );
    assert!(
        f.call(
            cx,
            "draft-2",
            "sessions.draft",
            json!({ "sessionId": "lead", "prompt": "Not here" })
        )
        .unwrap_err()
        .contains("composer")
    );
    assert!(
        f.call(
            cx,
            "draft-3",
            "sessions.draft",
            json!({ "sessionId": "missing", "prompt": "Not there" })
        )
        .unwrap_err()
        .contains("not found in this project")
    );
    assert_eq!(f.host.drafts.borrow().len(), 1);
}

#[gpui::test]
fn does_not_let_app_supplied_prompts_enable_operator_in_another_session(cx: &mut TestAppContext) {
    let f = fixture();
    for action in ["sessions.send", "sessions.draft", "sessions.start"] {
        for prompt in [
            "/operator list notes",
            "/mono list notes",
            "  /MONOCODE list notes",
        ] {
            let input = if action == "sessions.start" {
                json!({ "prompt": prompt })
            } else {
                json!({ "sessionId": "other", "prompt": prompt })
            };
            assert!(
                f.call(cx, "blocked", action, input)
                    .unwrap_err()
                    .contains("cannot enable /operator")
            );
        }
    }
    assert!(f.host.sends.borrow().is_empty());
    assert!(f.host.drafts.borrow().is_empty());
    assert!(f.host.starts.borrow().is_empty());
    f.call(
        cx,
        "ordinary",
        "sessions.send",
        json!({ "sessionId": "other", "prompt": "Explain the /operator command" }),
    )
    .unwrap();
    assert_eq!(f.host.sends.borrow().len(), 1);
}

#[gpui::test]
fn starts_a_submitted_tab_with_explicit_model_effort_permissions_and_workspace(
    cx: &mut TestAppContext,
) {
    let f = fixture();
    let result = f
        .call(
            cx,
            "request-1",
            "sessions.start",
            json!({
                "prompt": "Inspect the API",
                "model": "codex:test",
                "effort": "high",
                "runtimeMode": "auto-accept-edits",
                "workspaceMode": "worktree",
                "reveal": true
            }),
        )
        .unwrap();
    let starts = f.host.starts.borrow();
    let (launch, id, placement) = &starts[0];
    assert_eq!(launch.prompt, "Inspect the API");
    assert_eq!(launch.cwd, CWD);
    assert_eq!(launch.model, "codex:test");
    assert_eq!(
        launch.model_settings,
        [("effort".to_string(), "high".to_string())].into()
    );
    assert_eq!(launch.runtime_mode, RuntimeMode::AutoAcceptEdits);
    assert_eq!(
        launch.workspace_mode,
        monocode_core::session::WorkspaceMode::Worktree
    );
    assert!(launch.reveal);
    assert_eq!(id, "app-lead-request-1");
    assert_eq!(*placement, None);
    assert_eq!(result["id"], "app-lead-request-1");
    assert_eq!(result["submitted"], true);
}

#[gpui::test]
fn lists_project_worktrees_and_starts_on_a_selected_existing_checkout(cx: &mut TestAppContext) {
    let mut f = fixture();
    f.source.worktree_cwd = Some("/tmp/project-worktrees/other".into());
    let listed = f.call(cx, "list", "worktrees.list", json!({})).unwrap();
    assert_eq!(listed["worktrees"][0]["branch"], "feature");
    assert_eq!(*f.host.worktrees.borrow(), vec![CWD.to_string()]);
    f.call(
        cx,
        "feature",
        "sessions.start",
        json!({ "prompt": "Review feature", "worktreeCwd": feature_worktree().path }),
    )
    .unwrap();
    assert_eq!(
        f.host.starts.borrow()[0].0.worktree_cwd.as_deref(),
        Some("/tmp/project-worktrees/feature")
    );
    assert_eq!(f.host.starts.borrow()[0].1, "app-lead-feature");
    f.call(
        cx,
        "main",
        "sessions.start",
        json!({ "prompt": "Review main", "worktreeCwd": CWD }),
    )
    .unwrap();
    assert_eq!(f.host.starts.borrow()[1].0.worktree_cwd, None);
    assert_eq!(f.host.starts.borrow()[1].1, "app-lead-main");
}

#[gpui::test]
fn rejects_unavailable_or_conflicting_worktree_choices_before_launching(cx: &mut TestAppContext) {
    let f = fixture();
    for input in [
        json!({ "prompt": "A", "worktreeCwd": "/tmp/other-repo" }),
        json!({ "prompt": "A", "workspaceMode": "worktree", "worktreeCwd": feature_worktree().path }),
        json!({ "prompt": "A", "worktreeBase": "main", "worktreeCwd": feature_worktree().path }),
    ] {
        assert!(f.call(cx, "invalid", "sessions.start", input).is_err());
    }
    f.host.worktrees_once.borrow_mut().push_back(Worktrees {
        worktrees: vec![Worktree {
            missing: true,
            ..feature_worktree()
        }],
        default_root: "/tmp/project-worktrees".into(),
    });
    assert!(
        f.call(
            cx,
            "missing",
            "sessions.start",
            json!({ "prompt": "A", "worktreeCwd": feature_worktree().path })
        )
        .unwrap_err()
        .contains("unavailable in this project")
    );
    assert!(f.host.starts.borrow().is_empty());
}

#[gpui::test]
fn creates_a_worktree_on_a_named_new_or_existing_branch(cx: &mut TestAppContext) {
    let f = fixture();
    let created = f
        .call(
            cx,
            "new",
            "worktrees.create",
            json!({ "branch": "feature", "base": "origin/main" }),
        )
        .unwrap();
    assert_eq!(created["path"], feature_worktree().path);
    assert_eq!(
        f.host.created_worktrees.borrow()[0],
        (CWD.into(), "feature".into(), "origin/main".into(), false)
    );
    f.call(
        cx,
        "existing",
        "worktrees.create",
        json!({ "branch": "feature", "existing": true }),
    )
    .unwrap();
    assert_eq!(
        f.host.created_worktrees.borrow()[1],
        (CWD.into(), "feature".into(), "HEAD".into(), true)
    );
    assert!(
        f.call(
            cx,
            "bad",
            "worktrees.create",
            json!({ "branch": "feature", "base": "main", "existing": true })
        )
        .unwrap_err()
        .contains("base cannot be set")
    );
    assert_eq!(f.host.created_worktrees.borrow().len(), 2);
}

#[gpui::test]
fn starts_a_pane_beside_the_caller_or_another_session_in_either_direction(cx: &mut TestAppContext) {
    let f = fixture();
    f.call(
        cx,
        "right",
        "sessions.start",
        json!({ "prompt": "Inspect the API", "placement": "right", "draft": true }),
    )
    .unwrap();
    {
        let starts = f.host.starts.borrow();
        assert_eq!(starts[0].0.draft, Some(true));
        assert_eq!(starts[0].1, "app-lead-right");
        assert_eq!(
            starts[0].2,
            Some(AppSessionPlacement {
                direction: SplitDir::Right,
                beside_session_id: "lead".into(),
            })
        );
    }
    f.call(
        cx,
        "down",
        "sessions.start",
        json!({ "prompt": "Review the UI", "placement": "down", "besideSessionId": "app-lead-right" }),
    )
    .unwrap();
    let starts = f.host.starts.borrow();
    assert_eq!(starts[1].1, "app-lead-down");
    assert_eq!(
        starts[1].2,
        Some(AppSessionPlacement {
            direction: SplitDir::Down,
            beside_session_id: "app-lead-right".into(),
        })
    );
}

#[gpui::test]
fn rejects_invalid_pane_placement_before_starting(cx: &mut TestAppContext) {
    let f = fixture();
    for input in [
        json!({ "prompt": "A", "placement": "left" }),
        json!({ "prompt": "A", "besideSessionId": "other" }),
        json!({ "prompt": "A", "placement": "down", "besideSessionId": 42 }),
    ] {
        assert!(f.call(cx, "invalid", "sessions.start", input).is_err());
    }
    assert!(f.host.starts.borrow().is_empty());
}

#[gpui::test]
fn inherits_the_callers_permission_mode_unless_start_overrides_it(cx: &mut TestAppContext) {
    let mut f = fixture();
    f.source.runtime_mode = RuntimeMode::Auto;
    f.call(
        cx,
        "inherited-mode",
        "sessions.start",
        json!({ "prompt": "Review this" }),
    )
    .unwrap();
    assert_eq!(f.host.starts.borrow()[0].0.runtime_mode, RuntimeMode::Auto);
    assert_eq!(f.host.starts.borrow()[0].1, "app-lead-inherited-mode");
    f.call(
        cx,
        "explicit-mode",
        "sessions.start",
        json!({ "prompt": "Review this", "runtimeMode": "full-access" }),
    )
    .unwrap();
    assert_eq!(
        f.host.starts.borrow()[1].0.runtime_mode,
        RuntimeMode::FullAccess
    );
}

#[gpui::test]
fn starts_with_an_unsent_draft_and_can_immediately_move_the_new_session_into_a_folder(
    cx: &mut TestAppContext,
) {
    let f = fixture();
    let result = f
        .call(
            cx,
            "draft-launch",
            "sessions.start",
            json!({ "prompt": "Test prompt", "model": "codex:test", "draft": true }),
        )
        .unwrap();
    assert_eq!(result["id"], "app-lead-draft-launch");
    assert_eq!(result["submitted"], false);
    assert_eq!(result["draft"], true);
    assert_eq!(f.host.starts.borrow()[0].0.prompt, "Test prompt");
    assert_eq!(f.host.starts.borrow()[0].0.draft, Some(true));
    *f.host.listed.borrow_mut() = vec![AppSessionListing {
        id: "app-lead-draft-launch".into(),
        title: "Test prompt".into(),
        has_draft: true,
        ..other_listing()
    }];
    let moved = f
        .call(
            cx,
            "folder",
            "folders.move",
            json!({ "sessionId": "app-lead-draft-launch", "newFolderName": "test" }),
        )
        .unwrap();
    assert_eq!(moved["sessionId"], "app-lead-draft-launch");
    assert_eq!(moved["folderName"], "test");
}

#[gpui::test]
fn rejects_invalid_model_settings_before_starting_a_session(cx: &mut TestAppContext) {
    let f = fixture();
    assert!(
        f.call(
            cx,
            "bad",
            "sessions.start",
            json!({ "prompt": "Hello", "effort": "ultra" })
        )
        .unwrap_err()
        .contains("Invalid model setting effort")
    );
    assert!(f.host.starts.borrow().is_empty());
    assert!(
        f.call(
            cx,
            "bad-draft",
            "sessions.start",
            json!({ "prompt": "Hello", "draft": "true" })
        )
        .unwrap_err()
        .contains("draft must be a boolean")
    );
    assert!(
        f.call(cx, "unexpected", "toString", json!({}))
            .unwrap_err()
            .contains("Unknown app action")
    );
}

#[gpui::test]
fn moves_an_existing_project_session_into_a_sidebar_folder(cx: &mut TestAppContext) {
    let f = fixture();
    let kv = f.host.kv();
    save_session_folders(
        &kv,
        CWD,
        &[SessionFolder::new(
            "folder-1",
            "Research",
            vec!["lead".into()],
        )],
    );
    let moved = f
        .call(
            cx,
            "move",
            "folders.move",
            json!({ "sessionId": "other", "folderId": "folder-1" }),
        )
        .unwrap();
    assert_eq!(moved["folderId"], "folder-1");
    assert_eq!(moved["sessionId"], "other");
    assert_eq!(
        load_session_folders(&kv, CWD)[0].session_ids,
        vec!["lead".to_string(), "other".to_string()]
    );
    let listed = f.call(cx, "folders", "folders.list", json!({})).unwrap();
    assert_eq!(
        listed,
        json!({ "cwd": CWD, "folders": [{ "id": "folder-1", "name": "Research", "sessionIds": ["lead", "other"] }] })
    );
}

#[gpui::test]
fn creates_a_folder_during_a_move_and_rejects_unknown_sessions(cx: &mut TestAppContext) {
    let f = fixture();
    assert!(
        f.call(
            cx,
            "bad-move",
            "folders.move",
            json!({ "sessionId": "missing", "newFolderName": "Research" })
        )
        .unwrap_err()
        .contains("Session was not found")
    );
    let moved = f
        .call(
            cx,
            "new-folder",
            "folders.move",
            json!({ "sessionId": "other", "newFolderName": "Research" }),
        )
        .unwrap();
    assert_eq!(moved["folderName"], "Research");
    let folders = load_session_folders(&f.host.kv(), CWD);
    assert_eq!(json!(folders[0].id), moved["folderId"]);
    assert_eq!(folders[0].session_ids, vec!["other".to_string()]);
}

#[gpui::test]
fn lists_short_note_previews_and_reads_one_full_note_on_request(cx: &mut TestAppContext) {
    let f = fixture();
    let listed = f.call(cx, "list", "notes.list", json!({})).unwrap();
    assert_eq!(
        listed["notes"][0]["preview"],
        "First paragraph.\n\nSecond paragraph."
    );
    assert!(listed["notes"][0].get("body").is_none());
    assert_eq!(note_preview(&"A".repeat(500)).len(), 400);
    let read = f
        .call(cx, "read", "notes.read", json!({ "id": "n1" }))
        .unwrap();
    assert_eq!(read["body"], json!(note().body));
}

#[gpui::test]
fn creates_a_note_with_source_metadata_and_reuses_the_same_request_id_safely(
    cx: &mut TestAppContext,
) {
    let f = fixture();
    f.host.notes.borrow_mut().clear();
    let input = json!({ "body": "# Work plan\n\nNext steps", "tags": ["#Work"] });
    let saved = f
        .call(cx, "create-1", "notes.write", input.clone())
        .unwrap();
    assert_eq!(saved["id"], "app-lead-create-1");
    assert_eq!(saved["title"], "Work plan");
    assert_eq!(saved["body"], input["body"]);
    assert_eq!(saved["tags"], json!(["work"]));
    assert_eq!(saved["sourceSessionId"], "lead");
    assert_eq!(saved["sourceCwd"], CWD);
    assert_eq!(f.call(cx, "create-1", "notes.write", input).unwrap(), saved);
    assert_eq!(f.host.saved_notes.borrow().len(), 1);
    assert!(
        f.call(
            cx,
            "create-1",
            "notes.write",
            json!({ "body": "Different body" })
        )
        .unwrap_err()
        .contains("Request ID was already used")
    );
}

#[gpui::test]
fn edits_only_supplied_note_fields_and_refuses_missing_or_malformed_notes(cx: &mut TestAppContext) {
    let f = fixture();
    let changed = f
        .call(
            cx,
            "edit-1",
            "notes.write",
            json!({ "id": "n1", "body": "Updated body" }),
        )
        .unwrap();
    assert_eq!(changed["id"], "n1");
    assert_eq!(changed["title"], "Plan");
    assert_eq!(changed["body"], "Updated body");
    assert_eq!(changed["tags"], json!(["work"]));
    let upsert = f.host.saved_notes.borrow()[0].clone();
    assert_eq!(
        (upsert.id, upsert.title, upsert.body, upsert.tags),
        (
            "n1".into(),
            "Plan".into(),
            "Updated body".into(),
            vec!["work".to_string()]
        )
    );
    assert!(
        f.call(
            cx,
            "edit-2",
            "notes.write",
            json!({ "id": "missing", "body": "x" })
        )
        .unwrap_err()
        .contains("Note was not found")
    );
    assert!(
        f.call(cx, "edit-3", "notes.write", json!({ "id": "n1" }))
            .unwrap_err()
            .contains("Supply title, body or tags")
    );
    assert!(
        f.call(
            cx,
            "edit-4",
            "notes.write",
            json!({ "id": "n1", "tags": "work" })
        )
        .unwrap_err()
        .contains("tags must be an array")
    );
    assert!(
        f.call(cx, "create-2", "notes.write", json!({ "title": "Empty" }))
            .unwrap_err()
            .contains("body is required")
    );
}

#[gpui::test]
fn lists_models_with_settings_and_runtime_modes(cx: &mut TestAppContext) {
    let f = fixture();
    let listed = f.call(cx, "models", "models.list", json!({})).unwrap();
    assert_eq!(listed["runtimeModes"][0]["id"], "supervised");
    assert_eq!(listed["runtimeModes"][0]["label"], "Supervised");
    let codex = listed["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "codex")
        .unwrap();
    assert_eq!(codex["available"], true);
    assert_eq!(codex["models"][0]["id"], "codex:test");
    assert_eq!(codex["models"][0]["settings"][0]["id"], "effort");
    let sessions = f.call(cx, "sessions", "sessions.list", json!({})).unwrap();
    assert_eq!(sessions["cwd"], CWD);
    assert_eq!(sessions["sessions"][0]["id"], "other");
    assert_eq!(sessions["sessions"][0]["hasDraft"], false);
}

/// A thread without `/operator`, with the given settings and links.
fn restricted(open_sessions: bool, review_opened: bool, links: &[&str]) -> F {
    let mut source = Session::blank("lead", HarnessId::Codex, "codex:test", CWD);
    source.title = "Lead work".into();
    let host = FakeAppHost {
        listed: RefCell::new(vec![other_listing()]),
        kv: Some(Kv::in_memory()),
        open_sessions,
        review_opened,
        links: RefCell::new(links.iter().map(|id| (id.to_string(), 0)).collect()),
        ..Default::default()
    };
    F {
        source,
        host: Rc::new(host),
    }
}

#[gpui::test]
fn a_thread_without_operator_gets_only_the_open_session_actions(cx: &mut TestAppContext) {
    let f = restricted(true, false, &[]);
    let listed = f.call(cx, "list", "sessions.list", json!({})).unwrap();
    assert_eq!(listed["sessions"][0]["id"], "other");
    for (action, input) in [
        ("sessions.read", json!({ "sessionId": "other" })),
        (
            "sessions.send",
            json!({ "sessionId": "other", "prompt": "go" }),
        ),
        ("notes.list", json!({})),
        ("models.list", json!({})),
    ] {
        let error = f.call(cx, "denied", action, input).unwrap_err();
        assert!(error.contains("needs /operator"), "{action}: {error}");
    }
    assert!(
        f.call(cx, "links", "links.list", json!({}))
            .unwrap_err()
            .contains("no linked sessions")
    );
    assert!(f.host.sends.borrow().is_empty());
}

#[gpui::test]
fn the_setting_turns_open_session_actions_off(cx: &mut TestAppContext) {
    let f = restricted(false, false, &[]);
    for (action, input) in [
        ("sessions.list", json!({})),
        ("sessions.start", json!({ "prompt": "Investigate" })),
    ] {
        let error = f.call(cx, "off", action, input).unwrap_err();
        assert!(error.contains("Let agents open sessions"), "{error}");
    }
    assert!(f.host.starts.borrow().is_empty());
}

#[gpui::test]
fn a_restricted_start_runs_at_once_unless_review_is_on(cx: &mut TestAppContext) {
    let f = restricted(true, false, &[]);
    let started = f
        .call(
            cx,
            "start-1",
            "sessions.start",
            json!({ "prompt": "Investigate the bug" }),
        )
        .unwrap();
    assert_eq!(started["submitted"], true);
    assert_eq!(f.host.starts.borrow()[0].0.draft, None);
    let drafted = f
        .call(
            cx,
            "start-2",
            "sessions.start",
            json!({ "prompt": "Plan it", "draft": true }),
        )
        .unwrap();
    assert_eq!(drafted["draft"], true);
    assert_eq!(f.host.starts.borrow()[1].0.draft, Some(true));
    for field in [
        json!({ "prompt": "x", "runtimeMode": "full-access" }),
        json!({ "prompt": "x", "workspaceMode": "worktree" }),
    ] {
        assert!(
            f.call(cx, "start-3", "sessions.start", field)
                .unwrap_err()
                .contains("needs /operator")
        );
    }

    let review = restricted(true, true, &[]);
    let forced = review
        .call(
            cx,
            "start-4",
            "sessions.start",
            json!({ "prompt": "Investigate", "draft": false }),
        )
        .unwrap();
    assert_eq!(forced["draft"], true);
    assert_eq!(forced["submitted"], false);
    assert_eq!(review.host.starts.borrow()[0].0.draft, Some(true));
}

#[gpui::test]
fn operator_threads_ignore_the_review_setting(cx: &mut TestAppContext) {
    let mut f = fixture();
    Rc::get_mut(&mut f.host).unwrap().review_opened = true;
    let started = f
        .call(
            cx,
            "start-1",
            "sessions.start",
            json!({ "prompt": "Run now" }),
        )
        .unwrap();
    assert_eq!(started["submitted"], true);
    assert_eq!(f.host.starts.borrow()[0].0.draft, None);
}

#[gpui::test]
fn linked_sessions_read_and_message_each_other(cx: &mut TestAppContext) {
    let f = restricted(false, false, &["peer"]);
    let listed = f.call(cx, "list", "links.list", json!({})).unwrap();
    assert_eq!(listed["sessions"][0]["id"], "peer");
    assert_eq!(listed["sessions"][0]["messagesLeft"], 5);
    let page = f
        .call(cx, "read", "links.read", json!({ "sessionId": "peer" }))
        .unwrap();
    assert_eq!(page["turns"][0]["assistant"]["text"], "The API is done.");
    assert!(
        f.call(cx, "read-2", "links.read", json!({ "sessionId": "other" }))
            .unwrap_err()
            .contains("not linked")
    );
    let sent = f
        .call(
            cx,
            "send-1",
            "links.send",
            json!({ "sessionId": "peer", "prompt": "Please add tests." }),
        )
        .unwrap();
    assert_eq!(sent["submitted"], true);
    assert_eq!(sent["messagesLeft"], 4);
    assert_eq!(
        f.host.linked_sends.borrow()[0],
        (
            "peer".into(),
            "From linked session \"Lead work\" (lead):\n\nPlease add tests.".into(),
            "link-lead-send-1".into()
        )
    );
    assert!(
        f.call(
            cx,
            "send-2",
            "links.send",
            json!({ "sessionId": "peer", "prompt": "/operator do it" })
        )
        .unwrap_err()
        .contains("cannot enable /operator")
    );
    assert!(
        f.call(cx, "start", "sessions.start", json!({ "prompt": "x" }))
            .unwrap_err()
            .contains("Let agents open sessions")
    );
}

#[gpui::test]
fn a_busy_peer_queues_and_the_budget_stops_a_loop(cx: &mut TestAppContext) {
    let mut f = restricted(false, false, &["peer"]);
    Rc::get_mut(&mut f.host).unwrap().peer_busy = true;
    for index in 0..5 {
        let sent = f
            .call(
                cx,
                &format!("send-{index}"),
                "links.send",
                json!({ "sessionId": "peer", "prompt": "ping" }),
            )
            .unwrap();
        assert_eq!(sent["queued"], true);
        assert_eq!(sent["submitted"], false);
    }
    let error = f
        .call(
            cx,
            "send-6",
            "links.send",
            json!({ "sessionId": "peer", "prompt": "ping" }),
        )
        .unwrap_err();
    assert!(error.contains("5 agent messages"), "{error}");
    assert_eq!(f.host.linked_sends.borrow().len(), 5);
}

#[gpui::test]
fn a_failed_link_send_gives_its_message_back(cx: &mut TestAppContext) {
    let mut f = restricted(false, false, &["peer"]);
    Rc::get_mut(&mut f.host).unwrap().fail_linked_send = true;
    assert!(
        f.call(
            cx,
            "send-1",
            "links.send",
            json!({ "sessionId": "peer", "prompt": "ping" })
        )
        .is_err()
    );
    assert_eq!(f.host.links.borrow()[0].1, 0);
}
