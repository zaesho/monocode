//! Port of src/features/sessions/model/providerContext.ts: the native
//! provider conversations a session has used, and the delivery receipt for
//! the shared history a provider switch sends.
//!
//! A binding remembers one provider's native conversation for a working
//! directory and account, and the last transcript block that conversation
//! saw. A delivery tracks one switch from preparation through acceptance.
//! A startup id from the provider does not prove that it accepted a user
//! request, so acceptance is a separate step.
//!
//! The TypeScript returned new sessions. These functions change the session
//! in place.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::block::{
    BlockRole, HandoffTransfer, ModelSettings, ModelTarget, TransferMode, TransferStatus,
};
use crate::harness::HarnessId;
use crate::session::{PendingHarnessSwitch, Session, session_work_cwd};

/// `ProviderBinding`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderBinding {
    pub harness: HarnessId,
    pub provider_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_account_id: Option<String>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_through_block_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_used: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<i64>,
}

/// `ProviderContextDelivery`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderContextDelivery {
    pub switch_id: String,
    pub status: TransferStatus,
    pub mode: TransferMode,
    pub from: HarnessId,
    pub to: HarnessId,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_account_id: Option<String>,
    pub current_user_block_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_through_block_id: Option<String>,
    pub included_block_ids: Vec<String>,
    pub omitted_block_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_provider_session_id: Option<String>,
    /// Saved before dispatch, so a missing acknowledgment can mean the
    /// provider ran the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_submitted: Option<bool>,
    /// The caller confirmed that this failure happened before provider
    /// dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_before_submission: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_inspection: Option<bool>,
}

impl ProviderContextDelivery {
    pub fn is_submitted(&self) -> bool {
        self.request_submitted == Some(true)
    }

    pub fn needs_inspection(&self) -> bool {
        self.needs_inspection == Some(true)
    }

    /// Still on its way: preparing, or imported without acceptance.
    pub fn in_progress(&self) -> bool {
        matches!(
            self.status,
            TransferStatus::Preparing | TransferStatus::Imported
        )
    }
}

/// `ProviderContextState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderContextState {
    pub version: u32,
    pub bindings: Vec<ProviderBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<ProviderContextDelivery>,
}

impl Default for ProviderContextState {
    fn default() -> Self {
        Self {
            version: 1,
            bindings: Vec::new(),
            delivery: None,
        }
    }
}

/// What `beginProviderDelivery` takes: a delivery without status and mode.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeliveryStart {
    pub switch_id: String,
    pub from: Option<HarnessId>,
    pub to: Option<HarnessId>,
    pub cwd: String,
    pub provider_account_id: Option<String>,
    pub current_user_block_id: String,
    pub source_through_block_id: Option<String>,
    pub included_block_ids: Vec<String>,
    pub omitted_block_ids: Vec<String>,
    pub target_provider_session_id: Option<String>,
}

/// The coverage a delivery receipt reports.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeliveryCoverage {
    pub included_block_ids: Option<Vec<String>>,
    pub omitted_block_ids: Option<Vec<String>>,
    pub source_through_block_id: Option<String>,
}

/// `DEFAULT_PROVIDER_ACCOUNT_ID`, as `sameProviderAccountId` compares it.
const DEFAULT_ACCOUNT: &str = "default";

fn account(id: Option<&str>) -> &str {
    id.filter(|id| !id.is_empty()).unwrap_or(DEFAULT_ACCOUNT)
}

/// `sameProviderAccountId`, with empty ids read as the default account.
pub fn same_account(left: Option<&str>, right: Option<&str>) -> bool {
    account(left) == account(right)
}

fn same_selection(
    harness: HarnessId,
    cwd: &str,
    account_id: Option<&str>,
    other_harness: HarnessId,
    other_cwd: &str,
    other_account: Option<&str>,
) -> bool {
    harness == other_harness && cwd == other_cwd && same_account(account_id, other_account)
}

fn binding_matches(
    binding: &ProviderBinding,
    harness: HarnessId,
    cwd: &str,
    account_id: Option<&str>,
) -> bool {
    same_selection(
        binding.harness,
        &binding.cwd,
        binding.provider_account_id.as_deref(),
        harness,
        cwd,
        account_id,
    )
}

fn delivery_matches(
    delivery: &ProviderContextDelivery,
    harness: HarnessId,
    cwd: &str,
    account_id: Option<&str>,
) -> bool {
    same_selection(
        delivery.to,
        &delivery.cwd,
        delivery.provider_account_id.as_deref(),
        harness,
        cwd,
        account_id,
    )
}

fn last_block_id(session: &Session) -> Option<String> {
    session.blocks.last().map(|block| block.id.clone())
}

/// `runningProviderSelection`: the running turn keeps its provider and
/// model while the picker changes.
pub fn running_provider_selection(
    session: &Session,
    recorded: Option<&ModelTarget>,
) -> ModelTarget {
    if session.is_busy() {
        if let Some(recorded) = recorded {
            return recorded.clone();
        }
        if let Some(pending) = &session.pending_switch {
            return ModelTarget {
                harness: pending.from,
                model: pending.from_model.clone(),
                model_settings: pending.from_settings.clone(),
            };
        }
    }
    ModelTarget {
        harness: session.harness,
        model: session.model.clone(),
        model_settings: session.model_settings.clone(),
    }
}

/// `canApplyRunningConfiguration`: a running turn's model and context
/// reports describe the selection only when the picker has not moved.
/// `revisions` is `(running, selected)`.
pub fn can_apply_running_configuration(
    session: &Session,
    running: &ModelTarget,
    revisions: Option<(u64, u64)>,
) -> bool {
    if let Some((running_revision, selected_revision)) = revisions {
        return session.harness == running.harness && running_revision == selected_revision;
    }
    session.harness == running.harness
        && session.model == running.model
        && same_settings(&session.model_settings, &running.model_settings)
}

fn same_settings(left: &ModelSettings, right: &ModelSettings) -> bool {
    left.keys()
        .chain(right.keys())
        .all(|key| left.get(key) == right.get(key))
}

fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty() && !value.contains('\0'))
        .map(str::to_string)
}

fn harness(value: Option<&Value>) -> Option<HarnessId> {
    value.and_then(Value::as_str).and_then(HarnessId::parse)
}

fn usage(value: Option<&Value>) -> Option<i64> {
    let number = value?.as_f64()?;
    (number.is_finite() && number >= 0.0).then_some(number as i64)
}

fn ids(value: &[Value]) -> Vec<String> {
    let mut seen = HashSet::new();
    value
        .iter()
        .filter_map(|id| text(Some(id)))
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

fn flag(value: Option<&Value>) -> Option<bool> {
    (value == Some(&Value::Bool(true))).then_some(true)
}

/// `sanitizeProviderContext`: the saved state, keeping only well-formed
/// bindings and a well-formed delivery.
pub fn sanitize_provider_context(value: &Value) -> Option<ProviderContextState> {
    let state = value.as_object()?;
    if state.get("version").and_then(Value::as_u64) != Some(1) {
        return None;
    }
    let entries = state.get("bindings")?.as_array()?;
    let mut bindings: Vec<ProviderBinding> = Vec::new();
    for entry in entries {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let Some(harness) = harness(entry.get("harness")) else {
            continue;
        };
        let (Some(cwd), Some(provider_session_id)) =
            (text(entry.get("cwd")), text(entry.get("providerSessionId")))
        else {
            continue;
        };
        let binding = ProviderBinding {
            harness,
            cwd,
            provider_session_id,
            provider_account_id: text(entry.get("providerAccountId")),
            delivered_through_block_id: text(entry.get("deliveredThroughBlockId")),
            context_used: usage(entry.get("contextUsed")),
            context_window: usage(entry.get("contextWindow")).filter(|window| *window > 0),
        };
        match bindings.iter().position(|saved| {
            binding_matches(
                saved,
                binding.harness,
                &binding.cwd,
                binding.provider_account_id.as_deref(),
            )
        }) {
            Some(index) => bindings[index] = binding,
            None => bindings.push(binding),
        }
    }
    let delivery = state
        .get("delivery")
        .and_then(Value::as_object)
        .and_then(sanitize_delivery);
    Some(ProviderContextState {
        version: 1,
        bindings,
        delivery,
    })
}

fn sanitize_delivery(delivery: &Map<String, Value>) -> Option<ProviderContextDelivery> {
    let status: TransferStatus = serde_json::from_value(delivery.get("status")?.clone()).ok()?;
    let mode: TransferMode = serde_json::from_value(delivery.get("mode")?.clone()).ok()?;
    Some(ProviderContextDelivery {
        switch_id: text(delivery.get("switchId"))?,
        status,
        mode,
        from: harness(delivery.get("from"))?,
        to: harness(delivery.get("to"))?,
        cwd: text(delivery.get("cwd"))?,
        provider_account_id: text(delivery.get("providerAccountId")),
        current_user_block_id: text(delivery.get("currentUserBlockId"))?,
        source_through_block_id: text(delivery.get("sourceThroughBlockId")),
        included_block_ids: ids(delivery.get("includedBlockIds")?.as_array()?),
        omitted_block_ids: ids(delivery.get("omittedBlockIds")?.as_array()?),
        target_provider_session_id: text(delivery.get("targetProviderSessionId")),
        request_submitted: flag(delivery.get("requestSubmitted")),
        failed_before_submission: flag(delivery.get("failedBeforeSubmission")),
        needs_inspection: flag(delivery.get("needsInspection")),
    })
}

/// `isPersistableId`: the store's `validate_id`.
fn persistable_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// `sanitizePendingSwitch`.
pub fn sanitize_pending_switch(value: &Value) -> Option<PendingHarnessSwitch> {
    let candidate = value.as_object()?;
    let from = harness(candidate.get("from"))?;
    let from_model = candidate.get("fromModel")?.as_str()?.to_string();
    let from_settings = candidate
        .get("fromSettings")?
        .as_object()?
        .iter()
        .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
        .collect::<ModelSettings>();
    let id = |key: &str| {
        candidate
            .get(key)
            .and_then(Value::as_str)
            .filter(|id| persistable_id(id))
            .map(str::to_string)
    };
    Some(PendingHarnessSwitch {
        from,
        from_model,
        from_settings,
        from_provider_session_id: id("fromProviderSessionId"),
        from_provider_account_id: id("fromProviderAccountId"),
    })
}

/// `storedProviderContext`: the versioned envelope the store saves in
/// `provider_context_json`. `None` when there is nothing to save.
pub fn stored_provider_context(session: &Session) -> Option<Value> {
    let state = session
        .provider_context
        .as_ref()
        .and_then(|state| serde_json::to_value(state).ok())
        .and_then(|value| sanitize_provider_context(&value));
    let pending = session
        .pending_switch
        .as_ref()
        .and_then(|pending| serde_json::to_value(pending).ok())
        .and_then(|value| sanitize_pending_switch(&value));
    if state.is_none() && pending.is_none() {
        return None;
    }
    let mut envelope = Map::new();
    envelope.insert("version".into(), json!(1));
    if let Some(state) = state {
        envelope.insert("state".into(), serde_json::to_value(state).ok()?);
    }
    if let Some(pending) = pending {
        envelope.insert("pendingSwitch".into(), serde_json::to_value(pending).ok()?);
    }
    Some(Value::Object(envelope))
}

/// `restoreProviderContext`: the state and pending switch from a saved
/// envelope.
pub fn restore_provider_context(
    value: Option<&Value>,
) -> (Option<ProviderContextState>, Option<PendingHarnessSwitch>) {
    let Some(stored) = value.and_then(Value::as_object) else {
        return (None, None);
    };
    if stored.get("version").and_then(Value::as_u64) != Some(1) {
        return (None, None);
    }
    (
        stored.get("state").and_then(sanitize_provider_context),
        stored
            .get("pendingSwitch")
            .and_then(sanitize_pending_switch),
    )
}

/// `providerBinding`: the saved binding, or one inferred from the session's
/// own provider id or the pending switch's source.
pub fn provider_binding(
    session: &Session,
    harness: HarnessId,
    cwd: &str,
    account_id: Option<&str>,
) -> Option<ProviderBinding> {
    if let Some(saved) = session.provider_context.as_ref().and_then(|state| {
        state
            .bindings
            .iter()
            .find(|entry| binding_matches(entry, harness, cwd, account_id))
    }) {
        return Some(saved.clone());
    }
    if session.harness == harness
        && let Some(provider_session_id) = session
            .provider_session_id
            .as_ref()
            .filter(|id| !id.is_empty())
        && same_account(session.provider_account_id.as_deref(), account_id)
        && session_work_cwd(session) == cwd
    {
        return Some(ProviderBinding {
            harness,
            cwd: cwd.to_string(),
            provider_session_id: provider_session_id.clone(),
            provider_account_id: account_id.map(str::to_string),
            delivered_through_block_id: last_block_id(session),
            context_used: session.context.map(|context| context.used),
            context_window: session.context.and_then(|context| context.window),
        });
    }
    let source = session.pending_switch.as_ref()?;
    let provider_session_id = source
        .from_provider_session_id
        .as_ref()
        .filter(|id| !id.is_empty())?;
    (source.from == harness
        && same_account(source.from_provider_account_id.as_deref(), account_id)
        && session_work_cwd(session) == cwd)
        .then(|| ProviderBinding {
            harness,
            cwd: cwd.to_string(),
            provider_session_id: provider_session_id.clone(),
            provider_account_id: account_id.map(str::to_string),
            delivered_through_block_id: last_block_id(session),
            context_used: None,
            context_window: None,
        })
}

/// `canResumeProviderBinding`: the binding's saved boundary is still on the
/// transcript, so the missing interval can be computed.
pub fn can_resume_provider_binding(session: &Session, binding: Option<&ProviderBinding>) -> bool {
    binding
        .and_then(|binding| binding.delivered_through_block_id.as_deref())
        .is_some_and(|through| session.blocks.iter().any(|block| block.id == through))
}

/// `rememberProviderBinding`: replace the binding for the same provider,
/// directory, and account.
pub fn remember_provider_binding(session: &mut Session, binding: ProviderBinding) {
    let state = session
        .provider_context
        .get_or_insert_with(Default::default);
    state.bindings.retain(|entry| {
        !binding_matches(
            entry,
            binding.harness,
            &binding.cwd,
            binding.provider_account_id.as_deref(),
        )
    });
    state.bindings.push(binding);
}

/// `recordProviderBound`: a startup identity is separate from proof that
/// the target accepted a turn.
pub fn record_provider_bound(
    session: &mut Session,
    harness: HarnessId,
    cwd: &str,
    provider_session_id: &str,
    account_id: Option<&str>,
) {
    let saved = provider_binding(session, harness, cwd, account_id)
        .filter(|saved| saved.provider_session_id == provider_session_id);
    remember_provider_binding(
        session,
        ProviderBinding {
            harness,
            cwd: cwd.to_string(),
            provider_account_id: account_id.map(str::to_string),
            provider_session_id: provider_session_id.to_string(),
            delivered_through_block_id: saved
                .as_ref()
                .and_then(|saved| saved.delivered_through_block_id.clone()),
            context_used: saved.as_ref().and_then(|saved| saved.context_used),
            context_window: saved.as_ref().and_then(|saved| saved.context_window),
        },
    );
    match &mut session.pending_switch {
        Some(pending) if pending.from == harness && session.harness != harness => {
            pending.from_provider_session_id = Some(provider_session_id.to_string());
        }
        _ if session.harness == harness => {
            session.provider_session_id = Some(provider_session_id.to_string());
            session.provider_account_id = account_id.map(str::to_string);
        }
        _ => {}
    }
    if let Some(delivery) = session
        .provider_context
        .as_mut()
        .and_then(|state| state.delivery.as_mut())
        && delivery.status != TransferStatus::Accepted
        && delivery_matches(delivery, harness, cwd, account_id)
    {
        delivery.target_provider_session_id = Some(provider_session_id.to_string());
    }
}

/// `beginProviderDelivery`: start the receipt and mark the last handoff row
/// as preparing.
pub fn begin_provider_delivery(session: &mut Session, start: DeliveryStart) {
    let (Some(from), Some(to)) = (start.from, start.to) else {
        return;
    };
    if let Some(block) = session
        .blocks
        .iter_mut()
        .rev()
        .find(|block| block.handoff.is_some())
        && let Some(handoff) = &mut block.handoff
    {
        handoff.transfer = Some(HandoffTransfer {
            switch_id: start.switch_id.clone(),
            status: TransferStatus::Preparing,
            mode: TransferMode::Pending,
            included: start.included_block_ids.len() as u64,
            omitted: start.omitted_block_ids.len() as u64,
            historical_attachments: 0,
            retrieval_path: None,
            request_submitted: None,
            failed_before_submission: None,
            needs_inspection: None,
            inspection_confirmed: None,
        });
    }
    session
        .provider_context
        .get_or_insert_with(Default::default)
        .delivery = Some(ProviderContextDelivery {
        switch_id: start.switch_id,
        status: TransferStatus::Preparing,
        mode: TransferMode::Pending,
        from,
        to,
        cwd: start.cwd,
        provider_account_id: start.provider_account_id,
        current_user_block_id: start.current_user_block_id,
        source_through_block_id: start.source_through_block_id,
        included_block_ids: start.included_block_ids,
        omitted_block_ids: start.omitted_block_ids,
        target_provider_session_id: start.target_provider_session_id,
        request_submitted: None,
        failed_before_submission: None,
        needs_inspection: None,
    });
}

/// `updateProviderHandoff`: change the transfer details on the row for
/// `switch_id`.
pub fn update_provider_handoff(
    session: &mut Session,
    switch_id: &str,
    update: impl Fn(&mut HandoffTransfer),
) {
    for block in &mut session.blocks {
        if let Some(transfer) = block
            .handoff
            .as_mut()
            .and_then(|handoff| handoff.transfer.as_mut())
            .filter(|transfer| transfer.switch_id == switch_id)
        {
            update(transfer);
        }
    }
}

/// The open delivery for `switch_id`, when it is neither accepted nor
/// uncertain.
fn open_delivery<'a>(
    session: &'a mut Session,
    switch_id: &str,
) -> Option<&'a mut ProviderContextDelivery> {
    session
        .provider_context
        .as_mut()?
        .delivery
        .as_mut()
        .filter(|delivery| delivery.switch_id == switch_id && delivery.in_progress())
}

/// `markProviderContextDelivered`: the history reached the target. The
/// current request may still fail.
pub fn mark_provider_context_delivered(
    session: &mut Session,
    switch_id: &str,
    mode: TransferMode,
    provider_session_id: Option<&str>,
    coverage: DeliveryCoverage,
) {
    let Some(delivery) = open_delivery(session, switch_id) else {
        return;
    };
    if let Some(included) = &coverage.included_block_ids {
        delivery.included_block_ids = included.clone();
    }
    if let Some(omitted) = &coverage.omitted_block_ids {
        delivery.omitted_block_ids = omitted.clone();
    }
    if let Some(through) = &coverage.source_through_block_id {
        delivery.source_through_block_id = Some(through.clone());
    }
    delivery.status = TransferStatus::Imported;
    delivery.mode = mode;
    if let Some(id) = provider_session_id {
        delivery.target_provider_session_id = Some(id.to_string());
    }
    update_provider_handoff(session, switch_id, |transfer| {
        transfer.status = TransferStatus::Imported;
        transfer.mode = mode;
        if let Some(included) = &coverage.included_block_ids {
            transfer.included = included.len() as u64;
        }
        if let Some(omitted) = &coverage.omitted_block_ids {
            transfer.omitted = omitted.len() as u64;
        }
    });
}

/// `acceptProviderDelivery`: the target accepted the request, so the switch
/// is complete.
pub fn accept_provider_delivery(session: &mut Session, switch_id: &str) {
    let Some(delivery) = open_delivery(session, switch_id) else {
        return;
    };
    delivery.status = TransferStatus::Accepted;
    session.pending_switch = None;
    update_provider_handoff(session, switch_id, |transfer| {
        transfer.status = TransferStatus::Accepted;
    });
}

/// `markProviderRequestSubmitted`: save this marker before dispatching the
/// first request into the target.
pub fn mark_provider_request_submitted(session: &mut Session, switch_id: &str) {
    let Some(delivery) = open_delivery(session, switch_id) else {
        return;
    };
    if delivery.is_submitted() {
        return;
    }
    delivery.request_submitted = Some(true);
    delivery.failed_before_submission = None;
    update_provider_handoff(session, switch_id, |transfer| {
        transfer.request_submitted = Some(true);
        transfer.failed_before_submission = None;
    });
}

/// `recoverSubmittedProviderDelivery`: a saved dispatch marker cannot prove
/// whether the provider ran the request. Keep the request and its bindings
/// for inspection, and pause the queue.
pub fn recover_submitted_provider_delivery(session: &mut Session, switch_id: &str) {
    let Some(delivery) = open_delivery(session, switch_id) else {
        return;
    };
    if !delivery.is_submitted() {
        return;
    }
    delivery.status = TransferStatus::Uncertain;
    delivery.needs_inspection = Some(true);
    delivery.failed_before_submission = None;
    let user_id = delivery.current_user_block_id.clone();
    for block in &mut session.blocks {
        if block.id == user_id && block.role == BlockRole::User {
            block.draft = None;
        }
    }
    if session
        .queued_messages
        .as_ref()
        .is_some_and(|queue| !queue.is_empty())
    {
        session.queue_status = Some(crate::session::MessageQueueStatus::Paused);
    }
    update_provider_handoff(session, switch_id, |transfer| {
        transfer.status = TransferStatus::Uncertain;
        transfer.needs_inspection = Some(true);
        transfer.failed_before_submission = None;
    });
}

/// `confirmProviderDeliveryInspection`: record the user's inspection
/// without replaying the request or claiming acceptance.
pub fn confirm_provider_delivery_inspection(session: &mut Session) {
    let Some(delivery) = session
        .provider_context
        .as_ref()
        .and_then(|state| state.delivery.clone())
    else {
        return;
    };
    if !delivery.needs_inspection()
        || delivery.status != TransferStatus::Uncertain
        || session.is_busy()
    {
        return;
    }
    // Imported history does not prove the request reached the provider.
    // Keep its identity for inspection and rebuild full history next time.
    if let Some(target) = provider_binding(
        session,
        delivery.to,
        &delivery.cwd,
        delivery.provider_account_id.as_deref(),
    ) {
        remember_provider_binding(
            session,
            ProviderBinding {
                delivered_through_block_id: None,
                ..target
            },
        );
    }
    if let Some(state) = &mut session.provider_context {
        state.delivery = None;
    }
    update_provider_handoff(session, &delivery.switch_id, |transfer| {
        transfer.needs_inspection = None;
        transfer.inspection_confirmed = Some(true);
    });
    for block in &mut session.blocks {
        if let Some(handoff) = &mut block.handoff
            && handoff
                .transfer
                .as_ref()
                .is_some_and(|transfer| transfer.switch_id == delivery.switch_id)
        {
            handoff.pending = Some(false);
        }
    }
}

/// `failProviderDelivery`: the switch failed. The request goes back to a
/// draft and the target's binding is dropped, so a retry starts a fresh
/// conversation. `before_submission` records proof that nothing was sent.
pub fn fail_provider_delivery(session: &mut Session, switch_id: &str, before_submission: bool) {
    let Some(state) = &mut session.provider_context else {
        return;
    };
    let Some(delivery) = state.delivery.as_mut().filter(|delivery| {
        delivery.switch_id == switch_id && delivery.status != TransferStatus::Accepted
    }) else {
        return;
    };
    let proven_unsubmitted = before_submission && !delivery.needs_inspection();
    delivery.status = TransferStatus::Uncertain;
    if proven_unsubmitted {
        delivery.request_submitted = None;
        delivery.failed_before_submission = Some(true);
    }
    let delivery = delivery.clone();
    state.bindings.retain(|entry| {
        !binding_matches(
            entry,
            delivery.to,
            &delivery.cwd,
            delivery.provider_account_id.as_deref(),
        )
    });
    for block in &mut session.blocks {
        if block.id == delivery.current_user_block_id && block.role == BlockRole::User {
            block.draft = Some(true);
        }
    }
    if session.harness == delivery.to {
        session.provider_session_id = None;
    }
    update_provider_handoff(session, switch_id, |transfer| {
        transfer.status = TransferStatus::Uncertain;
        if proven_unsubmitted {
            transfer.request_submitted = None;
            transfer.failed_before_submission = Some(true);
        }
    });
}

/// `failUnstartedProviderRequest`: snapshot preparation may stop before a
/// delivery receipt exists. The request after the preparing divider goes
/// back to a draft.
pub fn fail_unstarted_provider_request(session: &mut Session, preparing_handoff_id: Option<&str>) {
    if session.pending_switch.is_none() {
        return;
    }
    let Some(divider) = session.blocks.iter().rposition(|block| {
        block
            .handoff
            .as_ref()
            .is_some_and(|handoff| handoff.status == crate::block::HandoffStatus::Preparing)
            || Some(block.id.as_str()) == preparing_handoff_id
    }) else {
        return;
    };
    if let Some(user) = session.blocks[divider + 1..]
        .iter_mut()
        .find(|block| block.role == BlockRole::User)
    {
        user.draft = Some(true);
    }
}

/// `requiresFreshProviderBinding`: an unaccepted delivery to this target
/// means its saved conversation may hold partial history.
pub fn requires_fresh_provider_binding(
    session: &Session,
    harness: HarnessId,
    cwd: &str,
    account_id: Option<&str>,
) -> bool {
    session
        .provider_context
        .as_ref()
        .and_then(|state| state.delivery.as_ref())
        .is_some_and(|delivery| {
            delivery.status != TransferStatus::Accepted
                && !delivery.needs_inspection()
                && delivery_matches(delivery, harness, cwd, account_id)
        })
}

/// `settleProviderBinding`: the provider has seen the transcript through
/// its last block.
pub fn settle_provider_binding(
    session: &mut Session,
    harness: HarnessId,
    cwd: &str,
    account_id: Option<&str>,
) {
    let Some(binding) = provider_binding(session, harness, cwd, account_id) else {
        return;
    };
    let usage = (session.harness == harness)
        .then_some(session.context)
        .flatten();
    let delivered_through_block_id = last_block_id(session);
    remember_provider_binding(
        session,
        ProviderBinding {
            delivered_through_block_id,
            context_used: match usage {
                Some(usage) => Some(usage.used),
                None => binding.context_used,
            },
            context_window: match usage {
                Some(usage) => usage.window,
                None => binding.context_window,
            },
            ..binding
        },
    );
}

/// `recordProviderContextUsage`.
pub fn record_provider_context_usage(
    session: &mut Session,
    harness: HarnessId,
    cwd: &str,
    used: Option<i64>,
    window: Option<i64>,
    account_id: Option<&str>,
) {
    let Some(mut binding) = provider_binding(session, harness, cwd, account_id) else {
        return;
    };
    if let Some(used) = used.filter(|used| *used >= 0) {
        binding.context_used = Some(used);
    }
    if let Some(window) = window.filter(|window| *window > 0) {
        binding.context_window = Some(window);
    }
    remember_provider_binding(session, binding);
}

/// `pendingSwitch.from`'s target for `withHarnessChoice`: keep the binding
/// the session is leaving.
pub fn remember_leaving_binding(session: &mut Session) {
    let (harness, account) = match &session.pending_switch {
        Some(pending) => (pending.from, pending.from_provider_account_id.clone()),
        None => (session.harness, session.provider_account_id.clone()),
    };
    let cwd = session_work_cwd(session).to_string();
    if let Some(binding) = provider_binding(session, harness, &cwd, account.as_deref()) {
        remember_provider_binding(session, binding);
    }
}

#[cfg(test)]
mod tests;
