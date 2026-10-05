//! Port of src/features/sessions/model/contextTransfer.ts: how a turn that
//! continues a conversation from another provider carries its shared
//! history, and when the target counts as having accepted the request.
//!
//! The registry wraps every turn with [`prepare_context_transfer_input`].
//! An adapter without native history import receives the history as
//! attributed text before the request. An adapter with native import (Codex)
//! receives the [`ContextTransferInput`] itself through
//! `HarnessAdapter::send_turn_with_context`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::FutureExt;
use futures::future::BoxFuture;
use parking_lot::Mutex;

use monocode_core::harness_event::{HarnessEvent, SendTurnInput};
use monocode_core::portable_context::{PortableContext, render_portable_context};

use super::registry::{AcceptedHook, EventSink};
use super::task::SharedSpawner;

/// `ContextTransferCapabilities`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContextTransferCapabilities {
    /// The adapter imports history as native conversation items.
    pub native_messages: bool,
    /// The adapter can resume a saved native conversation and append the
    /// history it lacks.
    pub resumed_append: bool,
    /// The adapter correlates acceptance evidence with the submitted request
    /// and calls `on_accepted` itself.
    pub explicit_acceptance: bool,
}

/// `ContextTransferReceipt["mode"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    Native,
    Inline,
}

/// `ContextTransferReceipt`: which history reached the target and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextTransferReceipt {
    pub mode: DeliveryMode,
    pub provider_session_id: Option<String>,
    pub included_ids: Option<Vec<String>>,
    pub omitted_ids: Option<Vec<String>>,
    pub through_block_id: Option<String>,
}

impl ContextTransferReceipt {
    /// The receipt for delivering all of `context`.
    pub fn for_context(
        mode: DeliveryMode,
        provider_session_id: Option<String>,
        context: &PortableContext,
    ) -> Self {
        Self {
            mode,
            provider_session_id,
            included_ids: Some(context.items.iter().map(|item| item.id.clone()).collect()),
            omitted_ids: Some(context.omitted.iter().map(|item| item.id.clone()).collect()),
            through_block_id: context.through_block_id.clone(),
        }
    }
}

/// `onDelivered`: history delivery succeeded. The current request may still
/// fail. An error means the receipt could not be saved.
pub type DeliveredHook =
    Arc<dyn Fn(ContextTransferReceipt) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// `ContextTransferInput`.
#[derive(Clone)]
pub struct ContextTransferInput {
    pub context: PortableContext,
    /// Full eligible history for a target whose native resume failed.
    pub fallback_context: Option<PortableContext>,
    pub on_delivered: Option<DeliveredHook>,
}

impl std::fmt::Debug for ContextTransferInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextTransferInput")
            .field("context", &self.context)
            .field("fallback_context", &self.fallback_context)
            .finish_non_exhaustive()
    }
}

/// `ContextTransferError`: some history may have reached the provider, so
/// retrying on the same native conversation could repeat it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextTransferError {
    pub message: String,
    pub cause: String,
}

impl std::fmt::Display for ContextTransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ContextTransferError {}

/// What an adapter receives after [`prepare_context_transfer_input`].
pub struct PreparedTurn {
    pub input: SendTurnInput,
    pub on_event: EventSink,
    pub on_accepted: Option<AcceptedHook>,
    /// Present only for an adapter that imports history natively.
    pub transfer: Option<ContextTransferInput>,
}

const DELIVERY_SAVE_FAILED: &str = "MonoCode could not save the shared-history delivery receipt.";

/// `reportInlineContextDelivery`: hand the receipt to the caller before
/// acceptance is reported, without waiting for its save. A failed save is
/// reported as a session error.
pub fn report_inline_context_delivery(
    transfer: &ContextTransferInput,
    receipt: ContextTransferReceipt,
    on_event: &EventSink,
    spawner: &SharedSpawner,
) {
    let Some(on_delivered) = transfer.on_delivered.as_ref() else {
        return;
    };
    // The hook runs now, so the caller records the receipt before the
    // acceptance that follows. Only its save runs later.
    let saved = on_delivered(receipt);
    let on_event = on_event.clone();
    spawner.spawn(
        async move {
            if let Err(error) = saved.await {
                on_event(HarnessEvent::SessionError {
                    message: format!("{DELIVERY_SAVE_FAILED} {error}"),
                });
            }
        }
        .boxed(),
    );
}

/// Events that show an adapter without acceptance evidence took the turn.
fn proves_acceptance(event: &HarnessEvent) -> bool {
    matches!(
        event,
        HarnessEvent::TurnStarted { .. }
            | HarnessEvent::MessageDelta { .. }
            | HarnessEvent::ToolStarted { .. }
            | HarnessEvent::Plan { .. }
            | HarnessEvent::ImageGenerated(_)
    )
}

/// `prepareContextTransferInput`: share acceptance evidence between the
/// desktop registry and the remote host. Acceptance is reported once. An
/// inline transfer renders the history into the request text and reports
/// its receipt on acceptance.
pub fn prepare_context_transfer_input(
    mut input: SendTurnInput,
    transfer: Option<ContextTransferInput>,
    capabilities: Option<ContextTransferCapabilities>,
    on_event: EventSink,
    on_accepted: Option<AcceptedHook>,
    spawner: SharedSpawner,
) -> PreparedTurn {
    let capabilities = capabilities.unwrap_or_default();
    if transfer.is_none() && on_accepted.is_none() {
        return PreparedTurn {
            input,
            on_event,
            on_accepted: None,
            transfer: None,
        };
    }
    let inline = transfer.clone().filter(|_| !capabilities.native_messages);
    if let Some(inline) = &inline {
        input.text = render_portable_context(&inline.context, &input.text);
    }
    let accepted = Arc::new(AtomicBool::new(false));
    let bound_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let wrapped_accepted: AcceptedHook = {
        let accepted = accepted.clone();
        let bound_id = bound_id.clone();
        let on_event = on_event.clone();
        Arc::new(move || {
            if accepted.swap(true, Ordering::SeqCst) {
                return;
            }
            if let Some(inline) = &inline {
                report_inline_context_delivery(
                    inline,
                    ContextTransferReceipt::for_context(
                        DeliveryMode::Inline,
                        bound_id.lock().clone(),
                        &inline.context,
                    ),
                    &on_event,
                    &spawner,
                );
            }
            if let Some(on_accepted) = &on_accepted {
                on_accepted();
            }
        })
    };
    let infer = !capabilities.native_messages && !capabilities.explicit_acceptance;
    let wrapped_event: EventSink = {
        let wrapped_accepted = wrapped_accepted.clone();
        Arc::new(move |event| {
            if let HarnessEvent::SessionProviderBound {
                provider_session_id,
            } = &event
            {
                *bound_id.lock() = Some(provider_session_id.clone());
            }
            if infer && proves_acceptance(&event) {
                wrapped_accepted();
            }
            on_event(event);
        })
    };
    PreparedTurn {
        input,
        on_event: wrapped_event,
        on_accepted: Some(wrapped_accepted),
        transfer: transfer.filter(|_| capabilities.native_messages),
    }
}

#[cfg(test)]
mod tests;
