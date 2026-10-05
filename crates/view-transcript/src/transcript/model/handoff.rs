//! The text `HandoffDivider` in AgentTranscript.tsx shows: the divider
//! label, its tooltip, and the lines under "Transfer details".

use monocode_core::block::{HandoffMeta, HandoffStatus, TransferMode, TransferStatus};

/// What a handoff divider shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffChrome {
    /// Draw the spinner and shimmer instead of the harness icon.
    pub preparing: bool,
    pub label: String,
    pub aria: String,
    /// The "Transfer details" lines, present only when the switch reported
    /// a transfer.
    pub details: Option<Vec<DetailLine>>,
}

/// One paragraph under "Transfer details". `code` follows `text` in a
/// monospace span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailLine {
    pub text: String,
    pub code: Option<String>,
}

impl DetailLine {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            code: None,
        }
    }
}

pub fn handoff_chrome(meta: &HandoffMeta) -> HandoffChrome {
    let title = meta.to.title();
    let transfer = meta.transfer.as_ref();
    let status = transfer.map(|transfer| transfer.status);
    let uncertain = status == Some(TransferStatus::Uncertain);
    let accepted = status == Some(TransferStatus::Accepted);
    let needs_inspection = transfer.is_some_and(|t| t.needs_inspection == Some(true));
    let inspected = transfer.is_some_and(|t| t.inspection_confirmed == Some(true));
    // An uncertain transfer is no longer in progress, even while the row
    // still says preparing.
    let preparing = meta.status == HandoffStatus::Preparing && !uncertain;
    let label = if needs_inspection {
        if accepted {
            "Acceptance needs saving".to_string()
        } else {
            "Execution needs inspection".to_string()
        }
    } else if inspected {
        "Execution inspected".to_string()
    } else if preparing {
        "Preparing shared history".to_string()
    } else if uncertain {
        "Handoff needs retry".to_string()
    } else if accepted {
        format!("Continued with {title}")
    } else if transfer.is_some() {
        format!("Starting {title}")
    } else {
        title.to_string()
    };
    let aria = if preparing {
        format!("Preparing a handoff to {title}")
    } else if transfer.is_some() {
        label.clone()
    } else {
        format!("Continued with {label}")
    };
    let details = transfer.map(|transfer| {
        let mut lines = vec![
            DetailLine::plain(format!(
                "{} conversation items selected. {} items omitted.",
                transfer.included, transfer.omitted
            )),
            DetailLine::plain(match transfer.mode {
                TransferMode::Native => {
                    "The provider received historical user and assistant messages."
                }
                TransferMode::Inline => {
                    "The provider received attributed history with the current request."
                }
                TransferMode::Pending => "History delivery is pending.",
            }),
        ];
        if transfer.historical_attachments > 0 {
            lines.push(DetailLine::plain(format!(
                "{} historical attachments are file references.",
                transfer.historical_attachments
            )));
        }
        if let Some(path) = &transfer.retrieval_path {
            lines.push(DetailLine {
                text: "The saved history is available at".to_string(),
                code: Some(path.clone()),
            });
        }
        if needs_inspection {
            lines.push(DetailLine::plain(if accepted {
                "The provider acknowledged this request, but MonoCode could not save its receipt. Restore saving before continuing."
            } else {
                "The request may have run. Inspect the provider conversation and changed files before continuing. MonoCode will not resend it automatically."
            }));
        }
        if inspected {
            lines.push(DetailLine::plain(
                "You confirmed inspection of this request. MonoCode did not resend it.",
            ));
        }
        if uncertain && !needs_inspection && !inspected {
            lines.push(DetailLine::plain(
                "The request was not confirmed. Retry the unsent message to continue.",
            ));
        }
        lines
    });
    HandoffChrome {
        preparing,
        label,
        aria,
        details,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monocode_core::HarnessId;
    use monocode_core::block::HandoffTransfer;

    fn transfer(status: TransferStatus, mode: TransferMode) -> HandoffTransfer {
        HandoffTransfer {
            switch_id: "switch-1".to_string(),
            status,
            mode,
            included: 2,
            omitted: 0,
            historical_attachments: 0,
            retrieval_path: None,
            request_submitted: None,
            failed_before_submission: None,
            needs_inspection: None,
            inspection_confirmed: None,
        }
    }

    fn meta(status: HandoffStatus, transfer: Option<HandoffTransfer>) -> HandoffMeta {
        HandoffMeta {
            from: HarnessId::Claude,
            to: HarnessId::Codex,
            status,
            pending: None,
            transfer,
            extra: Default::default(),
        }
    }

    /// The details as one string, the way a reader sees them.
    fn details_text(chrome: &HandoffChrome) -> String {
        chrome
            .details
            .as_ref()
            .expect("transfer details")
            .iter()
            .map(|line| match &line.code {
                Some(code) => format!("{} {code}.", line.text),
                None => line.text.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_submitted_uncertain_request_needs_inspection_not_a_retry() {
        for status in [HandoffStatus::Ready, HandoffStatus::Preparing] {
            let mut transfer = transfer(TransferStatus::Uncertain, TransferMode::Inline);
            transfer.request_submitted = Some(true);
            transfer.needs_inspection = Some(true);
            let chrome = handoff_chrome(&meta(status, Some(transfer)));
            assert_eq!(chrome.label, "Execution needs inspection");
            assert!(!chrome.preparing);
            let details = details_text(&chrome);
            assert!(details.contains("The request may have run"));
            assert!(details.contains("MonoCode will not resend it automatically"));
            assert!(!details.contains("Handoff needs retry"));
            assert!(!details.contains("unsent"));
        }
    }

    #[test]
    fn an_inspected_request_says_it_was_not_resent() {
        let mut transfer = transfer(TransferStatus::Uncertain, TransferMode::Native);
        transfer.request_submitted = Some(true);
        transfer.inspection_confirmed = Some(true);
        let chrome = handoff_chrome(&meta(HandoffStatus::Ready, Some(transfer)));
        assert_eq!(chrome.label, "Execution inspected");
        assert_ne!(chrome.label, "Continued with Codex");
        let details = details_text(&chrome);
        assert!(details.contains("MonoCode did not resend it"));
        assert!(!details.contains("Retry the unsent message"));
    }

    #[test]
    fn an_uncertain_transfer_asks_for_a_retry_with_full_details() {
        for status in [HandoffStatus::Ready, HandoffStatus::Preparing] {
            let mut transfer = transfer(TransferStatus::Uncertain, TransferMode::Native);
            transfer.included = 12;
            transfer.omitted = 3;
            transfer.historical_attachments = 2;
            transfer.retrieval_path = Some("/data/history/switch-1.md".to_string());
            let chrome = handoff_chrome(&meta(status, Some(transfer)));
            assert_eq!(chrome.label, "Handoff needs retry");
            assert!(!chrome.preparing);
            let details = details_text(&chrome);
            assert!(details.contains("12 conversation items selected"));
            assert!(details.contains("3 items omitted"));
            assert!(details.contains("2 historical attachments are file references"));
            assert!(details.contains("historical user and assistant messages"));
            assert!(details.contains("Retry the unsent message"));
            assert!(details.contains("/data/history/switch-1.md"));
        }
    }

    #[test]
    fn accepted_and_imported_transfers_name_the_next_provider() {
        let accepted = handoff_chrome(&meta(
            HandoffStatus::Ready,
            Some(transfer(TransferStatus::Accepted, TransferMode::Native)),
        ));
        assert_eq!(accepted.label, "Continued with Codex");
        assert_eq!(accepted.aria, "Continued with Codex");
        for status in [TransferStatus::Imported, TransferStatus::Preparing] {
            let chrome = handoff_chrome(&meta(
                HandoffStatus::Ready,
                Some(transfer(status, TransferMode::Pending)),
            ));
            assert_eq!(chrome.label, "Starting Codex");
            assert!(details_text(&chrome).contains("History delivery is pending."));
        }
        let preparing = handoff_chrome(&meta(
            HandoffStatus::Preparing,
            Some(transfer(TransferStatus::Preparing, TransferMode::Pending)),
        ));
        assert!(preparing.preparing);
        assert_eq!(preparing.label, "Preparing shared history");
        assert_eq!(preparing.aria, "Preparing a handoff to Codex");
    }

    #[test]
    fn a_row_without_a_transfer_keeps_its_tooltip_and_has_no_details() {
        let preparing = handoff_chrome(&meta(HandoffStatus::Preparing, None));
        assert!(preparing.preparing);
        assert_eq!(preparing.label, "Preparing shared history");
        assert_eq!(preparing.aria, "Preparing a handoff to Codex");
        assert_eq!(preparing.details, None);
        let ready = handoff_chrome(&meta(HandoffStatus::Ready, None));
        assert!(!ready.preparing);
        assert_eq!(ready.label, "Codex");
        assert_eq!(ready.aria, "Continued with Codex");
        assert_eq!(ready.details, None);
    }
}
