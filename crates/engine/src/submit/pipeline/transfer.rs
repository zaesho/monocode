//! The provider-switch half of the turn in App.tsx: export the shared
//! history the target lacks, save its receipt, and hand the target either
//! its own resumed conversation or a fresh one.

use gpui::AsyncApp;
use monocode_core::block::BlockRole;
use monocode_core::portable_context::{
    OmissionReason, PortableContext, PortableContextOptions, build_portable_context,
    build_portable_context_snapshot, current_attachment_tokens, historical_context_attachments,
};
use monocode_core::provider_context::{
    DeliveryStart, begin_provider_delivery, can_resume_provider_binding, provider_binding,
    remember_provider_binding, requires_fresh_provider_binding, update_provider_handoff,
};
use monocode_harness::core::context_transfer::ContextTransferInput;

use super::turn::{TurnRun, TurnState};
use crate::runtime::engine::Engine;
use crate::submit::handoff::complete_handoff;

fn budget_omitted(context: &PortableContext) -> bool {
    context
        .omitted
        .iter()
        .any(|entry| entry.reason == OmissionReason::Budget)
}

/// The brief on the handoff row once the history is ready.
fn transfer_brief(context: &PortableContext) -> String {
    let files: usize = context
        .items
        .iter()
        .map(|item| item.attachments.as_ref().map_or(0, Vec::len))
        .sum();
    let mut brief = format!(
        "Continue with shared history. {} saved items are prepared. {} items are omitted.",
        context.items.len(),
        context.omitted.len()
    );
    if let Some(path) = &context.retrieval_path {
        brief.push_str(&format!(" Saved history is available at {path}."));
    }
    if files > 0 {
        brief.push_str(&format!(
            " {files} historical attachments are file references."
        ));
    }
    brief
}

impl TurnRun {
    /// Prepare the shared history for a provider switch. `Ok(None)` means a
    /// newer turn took over. An error leaves the request unsent.
    pub(super) async fn prepare_transfer(
        &self,
        state: &mut TurnState,
        cx: &AsyncApp,
    ) -> Result<Option<()>, String> {
        let Some(pending) = self.pending_switch.clone() else {
            return Ok(Some(()));
        };
        let id = self.session_id.as_str();
        let registry = self.config.registry.clone();
        let current = &self.current;
        let harness = current.harness;
        let work_cwd = state.work_cwd.clone();
        let account = self.provider_account_id.clone();
        let saved_target = provider_binding(current, harness, &work_cwd, account.as_deref());
        // A target that can append to its own conversation gets only the
        // interval it missed. Anything uncertain starts fresh.
        let target = saved_target.filter(|saved| {
            registry.can_resume_harness_with_context(harness)
                && !requires_fresh_provider_binding(current, harness, &work_cwd, account.as_deref())
                && can_resume_provider_binding(current, Some(saved))
        });
        let fresh = target.is_none();
        let source_through = current.blocks.last().map(|block| block.id.clone());
        let writer = cx.update(|cx| Engine::writer(cx));
        let assets = writer
            .snapshot_context_assets(
                id,
                &historical_context_attachments(current, source_through.as_deref()),
            )
            .await?;
        if !self.gen_current(cx) {
            return Ok(None);
        }
        let window_tokens = self
            .config
            .catalog
            .read()
            .model_context_window(&current.model)
            .filter(|window| *window > 0)
            .or_else(|| target.as_ref().and_then(|target| target.context_window))
            .map(|window| window as usize);
        let base = PortableContextOptions {
            through_block_id: source_through.as_deref(),
            current_request: Some(&state.send_text),
            attachment_tokens: Some(current_attachment_tokens(&state.prepared)),
            window_tokens,
            asset_snapshots: &assets,
            ..Default::default()
        };
        let mut fallback = build_portable_context(current, &base)?;
        let mut context = match &target {
            Some(target) => build_portable_context(
                current,
                &PortableContextOptions {
                    after_block_id: target.delivered_through_block_id.as_deref(),
                    occupied_tokens: target.context_used.map(|used| used.max(0) as usize),
                    ..base.clone()
                },
            )?,
            None => fallback.clone(),
        };
        let switch_id = uuid::Uuid::new_v4().to_string();
        if budget_omitted(&fallback) || budget_omitted(&context) {
            let snapshot =
                build_portable_context_snapshot(current, source_through.as_deref(), &assets)?;
            let path = writer
                .save_switch_snapshot(id, &switch_id, snapshot)
                .await?;
            context.retrieval_path = Some(path.clone());
            fallback.retrieval_path = Some(path);
        }
        if !self.gen_current(cx) {
            return Ok(None);
        }
        self.flush(cx);
        let historical: u64 = context
            .items
            .iter()
            .map(|item| item.attachments.as_ref().map_or(0, Vec::len) as u64)
            .sum();
        let retrieval_path = context.retrieval_path.clone();
        let source = provider_binding(
            current,
            pending.from,
            &work_cwd,
            pending.from_provider_account_id.as_deref(),
        );
        let start = DeliveryStart {
            switch_id: switch_id.clone(),
            from: Some(pending.from),
            to: Some(harness),
            cwd: work_cwd.clone(),
            provider_account_id: account.clone(),
            current_user_block_id: String::new(),
            source_through_block_id: source_through.clone(),
            included_block_ids: context
                .items
                .iter()
                .map(|item| item.source_block_id.clone())
                .collect(),
            omitted_block_ids: context.omitted.iter().map(|item| item.id.clone()).collect(),
            target_provider_session_id: target
                .as_ref()
                .map(|target| target.provider_session_id.clone()),
        };
        self.update_session(cx, |session| {
            let Some(user) = session
                .blocks
                .iter()
                .rev()
                .find(|block| block.role == BlockRole::User)
                .map(|block| block.id.clone())
            else {
                return;
            };
            if let Some(source) = source {
                remember_provider_binding(session, source);
            }
            begin_provider_delivery(
                session,
                DeliveryStart {
                    current_user_block_id: user,
                    ..start
                },
            );
            update_provider_handoff(session, &switch_id, |transfer| {
                transfer.historical_attachments = historical;
                transfer.retrieval_path = retrieval_path.clone();
            });
        });
        if let Some(prepared) = self.latest(cx) {
            let _ = writer.upsert_session(&prepared).await;
        }
        if !self.gen_current(cx) {
            return Ok(None);
        }
        registry
            .stop_harness_session(pending.from, id)
            .await
            .map_err(|error| error.to_string())?;
        if fresh {
            registry
                .forget_harness_session(harness, id)
                .await
                .map_err(|error| error.to_string())?;
        }
        if !self.gen_current(cx) {
            return Ok(None);
        }
        if let Some(target) = &target {
            registry.bind_harness_session(
                harness,
                id,
                &target.provider_session_id,
                &work_cwd,
                account.as_deref(),
                Some(&current.blocks),
            );
        }
        let brief = transfer_brief(&context);
        state.transfer = Some(ContextTransferInput {
            context,
            fallback_context: Some(fallback),
            on_delivered: None,
        });
        state.transfer_switch_id = Some(switch_id);
        self.update_session(cx, |session| {
            *session = complete_handoff(session, &brief);
            session.busy = Some(true);
        });
        Ok(Some(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monocode_core::block::Block;
    use monocode_core::portable_context::export_portable_context;
    use monocode_core::{HarnessId, Session};

    #[test]
    fn the_brief_counts_items_attachments_and_the_saved_path() {
        let mut session = Session::blank("s", HarnessId::Codex, "codex:gpt", "/repo");
        session.blocks = vec![
            Block::new("u1", BlockRole::User, "One"),
            Block::new("a1", BlockRole::Assistant, "Two"),
        ];
        let mut context =
            export_portable_context(&session, &PortableContextOptions::default()).unwrap();
        assert_eq!(
            transfer_brief(&context),
            "Continue with shared history. 2 saved items are prepared. 0 items are omitted."
        );
        context.retrieval_path = Some("/data/switch.md".into());
        assert!(
            transfer_brief(&context).ends_with("Saved history is available at /data/switch.md.")
        );
    }
}
