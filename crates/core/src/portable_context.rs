//! Port of the export and budget half of the Tauri app's
//! src/features/sessions/model/portableContext.ts: a session's history as
//! JSON items another agent can read. Selection keeps whole messages, never
//! shortens one, and records what it left out and why.
//!
//! Provider switching sends this history to the next provider, and the
//! dropped-session context in the engine's `chat_context` uses the same
//! export.

use crate::attachment::AttachmentKind;
use crate::block::{BlockRole, PlanStatus, TurnModel};
use crate::{Attachment, Block, Session};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// `DEFAULT_HISTORY_BYTES`.
pub const DEFAULT_HISTORY_BYTES: usize = 16_000;
/// `MAX_HISTORY_BYTES`.
pub const MAX_HISTORY_BYTES: usize = 64_000;
/// The fixed metadata every selection starts with.
const BASE_COST: usize = 1_024;

/// `PortableAttachment`: a file reference. Its bytes never travel.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableAttachment {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub kind: AttachmentKind,
    pub size: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub delivery: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
}

/// `ContextAssetSnapshot`: a durable copy of one historical attachment, or
/// the reason it could not be saved.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextAssetSnapshot {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
}

/// `PortableContextItem["role"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PortableRole {
    User,
    Assistant,
}

/// `PortableContextItem`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableContextItem {
    pub id: String,
    pub source_block_id: String,
    pub source_role: BlockRole,
    pub role: PortableRole,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_model: Option<TurnModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<PortableAttachment>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Value>,
}

/// `PortableContextOmission["reason"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OmissionReason {
    Budget,
    PrivateReasoning,
    Draft,
    Unsettled,
    Internal,
    Status,
}

impl OmissionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            OmissionReason::Budget => "budget",
            OmissionReason::PrivateReasoning => "private-reasoning",
            OmissionReason::Draft => "draft",
            OmissionReason::Unsettled => "unsettled",
            OmissionReason::Internal => "internal",
            OmissionReason::Status => "status",
        }
    }
}

/// `PortableContextOmission`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PortableContextOmission {
    pub id: String,
    pub reason: OmissionReason,
}

/// `PortableContext`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableContext {
    pub version: u32,
    pub session_id: String,
    pub items: Vec<PortableContextItem>,
    pub omitted: Vec<PortableContextOmission>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub through_block_id: Option<String>,
    pub byte_length: usize,
    /// A saved transcript snapshot that the reading agent's file tools can
    /// open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieval_path: Option<String>,
}

/// `PortableContextOptions`.
#[derive(Debug, Clone, Default)]
pub struct PortableContextOptions<'a> {
    pub after_block_id: Option<&'a str>,
    pub through_block_id: Option<&'a str>,
    pub current_request: Option<&'a str>,
    /// Defaults to [`DEFAULT_HISTORY_BYTES`], capped at [`MAX_HISTORY_BYTES`].
    pub max_bytes: Option<usize>,
    pub window_tokens: Option<usize>,
    pub occupied_tokens: Option<usize>,
    pub attachment_tokens: Option<usize>,
    /// Durable attachment copies that replace the original paths.
    pub asset_snapshots: &'a [ContextAssetSnapshot],
    /// Export only user and assistant messages. Tool, plan, task, and notice
    /// rows are out of scope rather than omitted, so they get no omission
    /// entry. The TypeScript exporter has no such option.
    pub messages_only: bool,
}

fn bytes(value: &str) -> usize {
    value.len()
}

fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// `omissionReason`.
fn omission_reason(block: &Block) -> Option<OmissionReason> {
    if block.role == BlockRole::Reasoning {
        return Some(OmissionReason::PrivateReasoning);
    }
    if block.is_draft() {
        return Some(OmissionReason::Draft);
    }
    if block.is_internal() {
        return Some(OmissionReason::Internal);
    }
    let tool_running = matches!(block.role, BlockRole::Tool | BlockRole::Approval)
        && block
            .tool
            .as_ref()
            .and_then(|tool| tool.status.as_deref())
            .is_some_and(|status| matches!(status, "running" | "pending" | "in_progress"));
    if block.is_streaming()
        || block
            .plan
            .as_ref()
            .is_some_and(|plan| plan.status == PlanStatus::Streaming)
        || block
            .tool
            .as_ref()
            .is_some_and(|tool| tool.background == Some(true))
        || tool_running
        || (block.role == BlockRole::Approval
            && block
                .approval
                .as_ref()
                .is_none_or(|approval| approval.decided.is_none()))
    {
        return Some(OmissionReason::Unsettled);
    }
    if block.role == BlockRole::Handoff
        || (block.role == BlockRole::System && block.notice.is_none())
    {
        return Some(OmissionReason::Status);
    }
    None
}

/// `attachmentDescriptor`.
fn attachment_descriptor(attachment: &Attachment) -> PortableAttachment {
    PortableAttachment {
        id: attachment.id.clone(),
        name: attachment.name.clone(),
        mime_type: attachment.mime_type.clone(),
        kind: attachment.kind,
        size: attachment.size,
        path: attachment.path.clone().filter(|path| !path.is_empty()),
        delivery: "reference-only",
        sha256: None,
        unavailable_reason: None,
    }
}

fn insert(map: &mut Map<String, Value>, key: &str, value: impl Serialize) {
    if let Ok(value) = serde_json::to_value(value)
        && !value.is_null()
    {
        map.insert(key.to_string(), value);
    }
}

/// `exportItem`.
fn export_item(
    block: &Block,
    id: String,
    turn_model: Option<&TurnModel>,
) -> Option<PortableContextItem> {
    let mut attachments: Vec<PortableAttachment> = block
        .attachments
        .iter()
        .flatten()
        .map(attachment_descriptor)
        .collect();
    if let Some(image) = &block.image {
        attachments.push(PortableAttachment {
            id: block.id.clone(),
            name: image.name.clone(),
            mime_type: image.mime_type.clone(),
            kind: AttachmentKind::Image,
            size: image.size,
            path: Some(image.path.clone()).filter(|path| !path.is_empty()),
            delivery: "reference-only",
            sha256: None,
            unavailable_reason: None,
        });
    }
    let mut evidence: Option<Map<String, Value>> = None;
    match block.role {
        BlockRole::Tool | BlockRole::Approval => {
            let mut map = Map::new();
            if let Some(tool) = &block.tool {
                insert(&mut map, "kind", &tool.kind);
                insert(&mut map, "title", &tool.title);
                insert(&mut map, "status", &tool.status);
                insert(&mut map, "detail", &tool.detail);
                insert(&mut map, "preview", &tool.preview);
            }
            if let Some(decided) = block.approval.as_ref().and_then(|a| a.decided) {
                insert(&mut map, "approvalDecision", decided);
            }
            evidence = Some(map);
        }
        BlockRole::Tasks => {
            if let Some(list) = &block.task_list {
                let mut map = Map::new();
                insert(&mut map, "explanation", &list.explanation);
                insert(&mut map, "items", &list.items);
                evidence = Some(map);
            }
        }
        BlockRole::Plan => {
            if let Some(plan) = &block.plan
                && let Some(approved) = &plan.approved_text
            {
                let mut map = Map::new();
                insert(&mut map, "approvedText", approved);
                insert(&mut map, "status", plan.status);
                evidence = Some(map);
            }
        }
        BlockRole::System => {
            if let Some(notice) = block.notice {
                let mut map = Map::new();
                insert(&mut map, "notice", notice);
                evidence = Some(map);
            }
        }
        _ => {}
    }
    if let Some(ci) = &block.ci_context {
        evidence
            .get_or_insert_with(Map::new)
            .insert("ciContext".into(), Value::String(ci.clone()));
    }
    if block.text.is_empty() && attachments.is_empty() && evidence.is_none() {
        return None;
    }
    Some(PortableContextItem {
        id,
        source_block_id: block.id.clone(),
        source_role: block.role,
        role: if block.role == BlockRole::User {
            PortableRole::User
        } else {
            PortableRole::Assistant
        },
        text: block.text.clone(),
        turn_model: turn_model.cloned(),
        attachments: (!attachments.is_empty()).then_some(attachments),
        evidence: evidence.map(Value::Object),
    })
}

/// `exportPortableContext`: every eligible item, before budget selection.
pub fn export_portable_context(
    session: &Session,
    options: &PortableContextOptions<'_>,
) -> Result<PortableContext, String> {
    let blocks = &session.blocks;
    let through_index = match options.through_block_id {
        Some(through) => Some(
            blocks
                .iter()
                .position(|block| block.id == through)
                .ok_or("The frozen context boundary is missing from the transcript")?,
        ),
        None => blocks.len().checked_sub(1),
    };
    let after = options
        .after_block_id
        .and_then(|after| blocks.iter().position(|block| block.id == after));
    let start = after.map_or(0, |index| index + 1);
    let end = through_index.map_or(0, |index| index + 1);
    let mut context = PortableContext {
        version: 1,
        session_id: session.id.clone(),
        items: Vec::new(),
        omitted: Vec::new(),
        through_block_id: through_index.map(|index| blocks[index].id.clone()),
        byte_length: 0,
        retrieval_path: None,
    };
    // Attribution begins before the delta so the turn label carries over.
    let mut turn_model: Option<&TurnModel> = blocks[..start.min(blocks.len())]
        .iter()
        .rev()
        .find(|block| block.role == BlockRole::User && block.turn_model.is_some())
        .and_then(|block| block.turn_model.as_ref());
    let mut eligible: Vec<PortableContextItem> = Vec::new();
    for block in blocks.get(start..end).unwrap_or(&[]) {
        if block.role == BlockRole::User && block.turn_model.is_some() {
            turn_model = block.turn_model.as_ref();
        }
        let id = format!("{}:{}", session.id, block.id);
        if let Some(reason) = omission_reason(block) {
            context.omitted.push(PortableContextOmission { id, reason });
            continue;
        }
        if options.messages_only && !matches!(block.role, BlockRole::User | BlockRole::Assistant) {
            continue;
        }
        if let Some(item) = export_item(block, id, turn_model) {
            match eligible.iter().position(|entry| entry.id == item.id) {
                Some(previous) => eligible[previous] = item,
                None => eligible.push(item),
            }
        }
    }
    context.items = eligible;
    context.byte_length = portable_context_cost(&context);
    Ok(context)
}

fn history_budget(options: &PortableContextOptions<'_>) -> usize {
    let mut limit = options
        .max_bytes
        .unwrap_or(DEFAULT_HISTORY_BYTES)
        .min(MAX_HISTORY_BYTES);
    if let Some(window) = options.window_tokens.filter(|window| *window > 0) {
        let reserve = 16_000.max(window.div_ceil(4));
        // One UTF-8 byte per token overestimates the cost of ordinary text.
        let used = options.occupied_tokens.unwrap_or(0)
            + options.attachment_tokens.unwrap_or(0)
            + bytes(&to_json(&options.current_request.unwrap_or("")))
            + reserve;
        limit = limit.min(window.saturating_sub(used));
    }
    limit
}

/// `assertRequestCapacity`.
fn check_request_capacity(options: &PortableContextOptions<'_>) -> Result<(), String> {
    let Some(window) = options.window_tokens.filter(|window| *window > 0) else {
        return Ok(());
    };
    let available = window.saturating_sub(options.occupied_tokens.unwrap_or(0));
    let required = bytes(&to_json(&options.current_request.unwrap_or("")))
        + options.attachment_tokens.unwrap_or(0)
        + BASE_COST;
    if available <= required {
        return Err("The selected model does not have enough remaining context for this request and its history-transfer information. Compact the target conversation or choose a model with a larger context window.".into());
    }
    Ok(())
}

/// `currentAttachmentTokens`: the request's attachments as tokens, with
/// the path metadata providers add even for empty folders. URI escaping can
/// use three bytes for each source byte.
pub fn current_attachment_tokens(attachments: &[Attachment]) -> usize {
    attachments
        .iter()
        .map(|attachment| {
            let media = if attachment.kind == AttachmentKind::Image {
                4_000
            } else {
                16_000.min((attachment.size.max(0) as usize).div_ceil(2))
            };
            let path = match attachment.path.as_deref().filter(|path| !path.is_empty()) {
                Some(path) => {
                    128 + 3 * bytes(&to_json(&json!({
                        "kind": attachment.kind,
                        "name": attachment.name,
                        "mimeType": attachment.mime_type,
                        "path": path,
                    })))
                }
                None => 0,
            };
            media + path
        })
        .sum()
}

/// `historicalContextAttachments`: the attachments on exported history
/// through `through_block_id`, each id once.
pub fn historical_context_attachments(
    session: &Session,
    through_block_id: Option<&str>,
) -> Vec<Attachment> {
    let Ok(context) = export_portable_context(
        session,
        &PortableContextOptions {
            through_block_id,
            ..Default::default()
        },
    ) else {
        return Vec::new();
    };
    let eligible: std::collections::HashSet<&str> = context
        .items
        .iter()
        .map(|item| item.source_block_id.as_str())
        .collect();
    let mut attachments: Vec<Attachment> = Vec::new();
    let mut push = |attachment: Attachment| match attachments
        .iter()
        .position(|saved| saved.id == attachment.id)
    {
        Some(index) => attachments[index] = attachment,
        None => attachments.push(attachment),
    };
    for block in &session.blocks {
        if !eligible.contains(block.id.as_str()) {
            continue;
        }
        for attachment in block.attachments.iter().flatten() {
            push(attachment.clone());
        }
        if let Some(image) = &block.image {
            push(Attachment {
                id: block.id.clone(),
                kind: AttachmentKind::Image,
                name: image.name.clone(),
                mime_type: image.mime_type.clone(),
                size: image.size,
                path: Some(image.path.clone()).filter(|path| !path.is_empty()),
                ..Default::default()
            });
        }
    }
    attachments
}

/// `snapshotPortableContextAssets`: point each attachment at its durable
/// copy, or record why it is unavailable.
pub fn snapshot_portable_context_assets(
    mut context: PortableContext,
    snapshots: &[ContextAssetSnapshot],
) -> PortableContext {
    if snapshots.is_empty() {
        return context;
    }
    for item in &mut context.items {
        for attachment in item.attachments.iter_mut().flatten() {
            let Some(snapshot) = snapshots.iter().find(|saved| saved.id == attachment.id) else {
                continue;
            };
            attachment.path = snapshot
                .path
                .clone()
                .filter(|_| snapshot.unavailable_reason.is_none());
            if snapshot.sha256.is_some() {
                attachment.sha256 = snapshot.sha256.clone();
            }
            if snapshot.unavailable_reason.is_some() {
                attachment.unavailable_reason = snapshot.unavailable_reason.clone();
            }
        }
    }
    context.byte_length = portable_context_cost(&context);
    context
}

/// `buildPortableContext`: whole items within the byte budget. The last
/// user message, the last assistant message, and the first user message go
/// first, then the rest from newest to oldest. The chosen items keep their
/// original order.
pub fn build_portable_context(
    session: &Session,
    options: &PortableContextOptions<'_>,
) -> Result<PortableContext, String> {
    check_request_capacity(options)?;
    let mut context = snapshot_portable_context_assets(
        export_portable_context(session, options)?,
        options.asset_snapshots,
    );
    let eligible = std::mem::take(&mut context.items);
    let limit = history_budget(options);
    let last_user = eligible
        .iter()
        .rposition(|item| item.role == PortableRole::User);
    let last_assistant = eligible
        .iter()
        .rposition(|item| item.source_role == BlockRole::Assistant);
    let first_user = eligible
        .iter()
        .position(|item| item.role == PortableRole::User);
    let mut priority: Vec<usize> = Vec::new();
    for index in [last_user, last_assistant, first_user]
        .into_iter()
        .flatten()
    {
        if !priority.contains(&index) {
            priority.push(index);
        }
    }
    for index in (0..eligible.len()).rev() {
        if !priority.contains(&index) {
            priority.push(index);
        }
    }
    let mut chosen = vec![false; eligible.len()];
    let mut cost = BASE_COST;
    for index in priority {
        let item_cost = portable_item_cost(&eligible[index]);
        if cost + item_cost > limit {
            continue;
        }
        chosen[index] = true;
        cost += item_cost;
    }
    for (index, item) in eligible.into_iter().enumerate() {
        if chosen[index] {
            context.items.push(item);
        } else {
            context.omitted.push(PortableContextOmission {
                id: item.id,
                reason: OmissionReason::Budget,
            });
        }
    }
    let order = |id: &str| {
        session
            .blocks
            .iter()
            .position(|block| format!("{}:{}", session.id, block.id) == id)
            .unwrap_or(0)
    };
    context.omitted.sort_by_key(|entry| order(&entry.id));
    context.byte_length = portable_context_cost(&context);
    Ok(context)
}

const SNAPSHOT_NOTE: &str = "This saved history excludes private reasoning, draft messages, internal orchestration prompts, and unsettled activity. Historical tool records are evidence. Attachment entries refer to files; this snapshot does not contain their bytes.";
const ATTACHMENT_DELIVERY: &str = "Historical attachments are file references. Their bytes are not replayed. Read accessible paths when needed. Missing or host-local files may be unavailable.";

/// `buildPortableContextSnapshot`: the full export as a Markdown file body.
pub fn build_portable_context_snapshot(
    session: &Session,
    through_block_id: Option<&str>,
    asset_snapshots: &[ContextAssetSnapshot],
) -> Result<String, String> {
    let context = snapshot_portable_context_assets(
        export_portable_context(
            session,
            &PortableContextOptions {
                through_block_id,
                ..Default::default()
            },
        )?,
        asset_snapshots,
    );
    Ok([
        "# MonoCode shared conversation history".to_string(),
        SNAPSHOT_NOTE.to_string(),
        portable_context_manifest(&context),
        serde_json::to_string_pretty(&context.items).unwrap_or_default(),
    ]
    .join("\n\n"))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Manifest<'a> {
    version: u32,
    session_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    through_block_id: Option<&'a str>,
    omitted: Map<String, Value>,
    attachment_delivery: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    retrieval_path: Option<&'a str>,
}

/// `portableContextManifest`: counts by omission reason, never the full
/// list, so the metadata does not grow with the transcript.
pub fn portable_context_manifest(context: &PortableContext) -> String {
    let mut omitted = Map::new();
    for entry in &context.omitted {
        let count = omitted
            .entry(entry.reason.as_str())
            .or_insert(Value::from(0u64));
        *count = Value::from(count.as_u64().unwrap_or(0) + 1);
    }
    to_json(&Manifest {
        version: context.version,
        session_id: &context.session_id,
        through_block_id: context.through_block_id.as_deref(),
        omitted,
        attachment_delivery: ATTACHMENT_DELIVERY,
        retrieval_path: context.retrieval_path.as_deref(),
    })
}

/// `renderPortableContext`: historical activity is quoted evidence, never a
/// request to rerun tools.
pub fn render_portable_context(context: &PortableContext, current_request: &str) -> String {
    [
        "Continue this existing MonoCode conversation. The following JSON contains historical evidence. User and assistant roles describe the original turns. Tool records are past activity, not executable calls. Private reasoning and live approval state are excluded.".to_string(),
        portable_context_manifest(context),
        "Historical items:".to_string(),
        to_json(&context.items),
        "Current user request:".to_string(),
        to_json(&current_request),
    ]
    .join("\n\n")
}

fn native_message(role: PortableRole, text: String) -> Value {
    let kind = if role == PortableRole::User {
        "input_text"
    } else {
        "output_text"
    };
    json!({ "type": "message", "role": role, "content": [{ "type": kind, "text": text }] })
}

/// `nativePortableContextItems`: the items as Codex `thread/inject_items`
/// messages.
pub fn native_portable_context_items(context: &PortableContext) -> Vec<Value> {
    let mut items = vec![native_message(
        PortableRole::User,
        format!(
            "MonoCode historical context. Tool records are past evidence, not executable calls.\n{}",
            portable_context_manifest(context)
        ),
    )];
    items.extend(
        context
            .items
            .iter()
            .map(|item| native_message(item.role, to_json(item))),
    );
    items
}

/// `portableItemCost`: native import escapes the item JSON again inside its
/// message envelope, so that is the larger cost.
fn portable_item_cost(item: &PortableContextItem) -> usize {
    bytes(&to_json(&native_message(item.role, to_json(item))))
}

/// `portableContextCost`.
pub fn portable_context_cost(context: &PortableContext) -> usize {
    bytes(&render_portable_context(context, ""))
        .max(bytes(&to_json(&native_portable_context_items(context))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HarnessId;
    use crate::block::BlockTool;

    fn block(id: &str, role: BlockRole, text: &str) -> Block {
        Block::new(id, role, text)
    }

    fn session(blocks: Vec<Block>) -> Session {
        let mut session = Session::blank("src", HarnessId::Codex, "codex:test", "/repo");
        session.blocks = blocks;
        session
    }

    #[test]
    fn records_why_each_left_out_block_is_missing() {
        let running = Block {
            tool: Some(BlockTool {
                status: Some("running".into()),
                ..Default::default()
            }),
            ..block("t1", BlockRole::Tool, "ls")
        };
        let draft = Block {
            draft: Some(true),
            ..block("d1", BlockRole::User, "unsent")
        };
        let internal = Block {
            internal: Some(true),
            ..block("i1", BlockRole::User, "keep going")
        };
        let streaming = Block {
            streaming: Some(true),
            ..block("s1", BlockRole::Assistant, "partial")
        };
        let context = export_portable_context(
            &session(vec![
                block("u1", BlockRole::User, "hello"),
                block("r1", BlockRole::Reasoning, "secret"),
                running,
                draft,
                internal,
                streaming,
                block("h1", BlockRole::Handoff, "brief"),
                block("a1", BlockRole::Assistant, "hi"),
            ]),
            &PortableContextOptions::default(),
        )
        .unwrap();
        let reasons: Vec<(&str, OmissionReason)> = context
            .omitted
            .iter()
            .map(|entry| (entry.id.as_str(), entry.reason))
            .collect();
        assert_eq!(
            reasons,
            vec![
                ("src:r1", OmissionReason::PrivateReasoning),
                ("src:t1", OmissionReason::Unsettled),
                ("src:d1", OmissionReason::Draft),
                ("src:i1", OmissionReason::Internal),
                ("src:s1", OmissionReason::Unsettled),
                ("src:h1", OmissionReason::Status),
            ]
        );
        assert_eq!(
            context
                .items
                .iter()
                .map(|i| i.text.as_str())
                .collect::<Vec<_>>(),
            ["hello", "hi"]
        );
        assert_eq!(context.through_block_id.as_deref(), Some("a1"));
    }

    #[test]
    fn keeps_messages_whole_and_fills_the_budget_newest_first() {
        let mut blocks = Vec::new();
        for index in 1..=8 {
            blocks.push(block(
                &format!("u{index}"),
                BlockRole::User,
                &format!("question {index} {}", "x".repeat(900)),
            ));
            blocks.push(block(
                &format!("a{index}"),
                BlockRole::Assistant,
                &format!("answer {index} {}", "y".repeat(900)),
            ));
        }
        let source = session(blocks);
        let context = build_portable_context(
            &source,
            &PortableContextOptions {
                max_bytes: Some(6_000),
                ..Default::default()
            },
        )
        .unwrap();
        let ids: Vec<&str> = context
            .items
            .iter()
            .map(|i| i.source_block_id.as_str())
            .collect();
        // The first question, then the newest exchanges, in transcript order.
        assert_eq!(ids, ["u1", "a7", "u8", "a8"]);
        assert!(context.items.iter().all(|item| item.text.len() > 900));
        assert!(
            context
                .omitted
                .iter()
                .all(|entry| entry.reason == OmissionReason::Budget)
        );
        assert_eq!(
            context.omitted.first().map(|e| e.id.as_str()),
            Some("src:a1")
        );
        assert_eq!(context.omitted.len(), 12);
        let manifest: Value = serde_json::from_str(&portable_context_manifest(&context)).unwrap();
        assert_eq!(manifest["omitted"]["budget"], 12);
        assert_eq!(manifest["sessionId"], "src");
    }

    #[test]
    fn caps_the_budget_and_keeps_attachments_as_references() {
        let user = Block {
            attachments: Some(vec![Attachment {
                id: "f1".into(),
                name: "shot.png".into(),
                mime_type: "image/png".into(),
                kind: AttachmentKind::Image,
                size: 10,
                path: Some("/data/shot.png".into()),
                data: Some("AAAA".into()),
                ..Default::default()
            }]),
            ..block("u1", BlockRole::User, "see this")
        };
        let context = build_portable_context(
            &session(vec![user]),
            &PortableContextOptions {
                max_bytes: Some(1_000_000),
                ..Default::default()
            },
        )
        .unwrap();
        let attachment = &context.items[0].attachments.as_ref().unwrap()[0];
        assert_eq!(attachment.delivery, "reference-only");
        assert_eq!(attachment.path.as_deref(), Some("/data/shot.png"));
        assert!(!to_json(&context.items).contains("AAAA"));
        assert_eq!(
            history_budget(&PortableContextOptions {
                max_bytes: Some(1_000_000),
                ..Default::default()
            }),
            MAX_HISTORY_BYTES
        );
    }

    #[test]
    fn messages_only_leaves_out_tool_rows_without_an_omission() {
        let tool = Block {
            tool: Some(BlockTool {
                status: Some("completed".into()),
                kind: Some("shell".into()),
                ..Default::default()
            }),
            ..block("t1", BlockRole::Tool, "cargo test")
        };
        let source = session(vec![block("u1", BlockRole::User, "run"), tool]);
        let all = export_portable_context(&source, &PortableContextOptions::default()).unwrap();
        assert_eq!(all.items.len(), 2);
        assert_eq!(all.items[1].evidence.as_ref().unwrap()["kind"], "shell");
        let messages = export_portable_context(
            &source,
            &PortableContextOptions {
                messages_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(messages.items.len(), 1);
        assert!(messages.omitted.is_empty());
    }

    #[test]
    fn renders_like_the_desktop_exporter() {
        let mut context = export_portable_context(
            &session(vec![block("u1", BlockRole::User, "hello")]),
            &PortableContextOptions::default(),
        )
        .unwrap();
        context.retrieval_path = Some("/snap.md".into());
        let rendered = render_portable_context(&context, "next");
        assert!(rendered.starts_with("Continue this existing MonoCode conversation."));
        assert!(rendered.contains("\"retrievalPath\":\"/snap.md\""));
        assert!(rendered.contains("Historical items:\n\n[{\"id\":\"src:u1\",\"sourceBlockId\":\"u1\",\"sourceRole\":\"user\",\"role\":\"user\",\"text\":\"hello\"}]"));
        assert!(rendered.ends_with("Current user request:\n\n\"next\""));
        let snapshot = build_portable_context_snapshot(
            &session(vec![block("u1", BlockRole::User, "hello")]),
            None,
            &[],
        )
        .unwrap();
        assert!(snapshot.starts_with("# MonoCode shared conversation history"));
        assert!(snapshot.contains("\"text\": \"hello\""));
        assert!(
            export_portable_context(
                &session(vec![]),
                &PortableContextOptions {
                    through_block_id: Some("missing"),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }

    fn file(id: &str, path: Option<&str>) -> Attachment {
        Attachment {
            id: id.into(),
            name: format!("{id}.txt"),
            mime_type: "text/plain".into(),
            kind: AttachmentKind::File,
            size: 4,
            path: path.map(str::to_string),
            data: Some("bytes".into()),
            ..Default::default()
        }
    }

    #[test]
    fn collects_attachments_only_from_settled_history_before_the_boundary() {
        let with = |block: Block, id: &str| Block {
            attachments: Some(vec![file(id, None)]),
            ..block
        };
        let generated = Block {
            image: Some(crate::block::GeneratedImageMeta {
                path: "/original/generated.png".into(),
                name: "generated.png".into(),
                mime_type: "image/png".into(),
                size: 4,
                alt: None,
                extra: Default::default(),
            }),
            ..block("generated", BlockRole::Image, "")
        };
        let source = session(vec![
            with(block("u", BlockRole::User, "submitted"), "user-file"),
            with(
                Block {
                    draft: Some(true),
                    ..block("draft", BlockRole::User, "draft")
                },
                "draft-file",
            ),
            with(
                Block {
                    internal: Some(true),
                    ..block("internal", BlockRole::User, "internal")
                },
                "internal-file",
            ),
            generated,
            with(block("future", BlockRole::User, "later"), "future-file"),
        ]);
        let ids: Vec<String> = historical_context_attachments(&source, Some("generated"))
            .into_iter()
            .map(|attachment| attachment.id)
            .collect();
        assert_eq!(ids, ["user-file", "generated"]);
    }

    #[test]
    fn rewrites_references_to_saved_copies_without_touching_the_transcript() {
        let source = session(vec![Block {
            attachments: Some(vec![file("a", Some("/temporary/file.txt"))]),
            ..block("u", BlockRole::User, "Inspect the file")
        }]);
        let context = build_portable_context(&source, &PortableContextOptions::default()).unwrap();
        let snapshots = [ContextAssetSnapshot {
            id: "a".into(),
            path: Some("/app-data/context-history/src/assets/hash.txt".into()),
            sha256: Some("hash".into()),
            unavailable_reason: None,
        }];
        let saved = snapshot_portable_context_assets(context.clone(), &snapshots);
        let attachment = &saved.items[0].attachments.as_ref().unwrap()[0];
        assert_eq!(attachment.path, snapshots[0].path);
        assert_eq!(attachment.sha256.as_deref(), Some("hash"));
        assert_eq!(
            context.items[0].attachments.as_ref().unwrap()[0]
                .path
                .as_deref(),
            Some("/temporary/file.txt")
        );
        assert!(
            build_portable_context_snapshot(&source, Some("u"), &snapshots)
                .unwrap()
                .contains("/app-data/context-history/src/assets/hash.txt")
        );
        let unavailable = snapshot_portable_context_assets(
            context,
            &[ContextAssetSnapshot {
                id: "a".into(),
                unavailable_reason: Some("The source file is missing".into()),
                ..Default::default()
            }],
        );
        let attachment = &unavailable.items[0].attachments.as_ref().unwrap()[0];
        assert!(attachment.path.is_none());
        assert_eq!(
            attachment.unavailable_reason.as_deref(),
            Some("The source file is missing")
        );
    }

    #[test]
    fn counts_saved_paths_before_selecting_history() {
        let source = session(vec![Block {
            attachments: Some(vec![file("a", Some("/short"))]),
            ..block("u", BlockRole::User, "Inspect the file")
        }]);
        let options = PortableContextOptions {
            max_bytes: Some(1_800),
            ..Default::default()
        };
        assert_eq!(
            build_portable_context(&source, &options)
                .unwrap()
                .items
                .len(),
            1
        );
        let snapshots = [ContextAssetSnapshot {
            id: "a".into(),
            path: Some(format!("/{}asset.txt", "long-path-segment/".repeat(200))),
            sha256: Some("hash".into()),
            unavailable_reason: None,
        }];
        let budgeted = build_portable_context(
            &source,
            &PortableContextOptions {
                asset_snapshots: &snapshots,
                ..options
            },
        )
        .unwrap();
        assert!(budgeted.items.is_empty());
        assert_eq!(budgeted.omitted[0].reason, OmissionReason::Budget);
    }

    #[test]
    fn rejects_long_zero_size_folder_references_before_history_import() {
        let attachments: Vec<Attachment> = (0..20)
            .map(|index| Attachment {
                id: format!("folder-{index}"),
                name: format!("folder-{index}"),
                mime_type: "inode/directory".into(),
                kind: AttachmentKind::File,
                size: 0,
                path: Some(format!(
                    "/tmp/{}{}/{index}",
                    "nested-folder/".repeat(8),
                    "多字节目录".repeat(10)
                )),
                ..Default::default()
            })
            .collect();
        let source = session(vec![block("history", BlockRole::User, "Retain this.")]);
        let result = build_portable_context(
            &source,
            &PortableContextOptions {
                window_tokens: Some(20_000),
                occupied_tokens: Some(18_800),
                current_request: Some("Continue"),
                attachment_tokens: Some(current_attachment_tokens(&attachments)),
                ..Default::default()
            },
        );
        assert!(result.unwrap_err().contains("remaining context"));
        let image = Attachment {
            kind: AttachmentKind::Image,
            ..file("image", None)
        };
        assert_eq!(current_attachment_tokens(&[image]), 4_000);
    }
}
