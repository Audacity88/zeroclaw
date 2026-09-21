//! Failed-turn media degradation, shared by the ACP restore and live-session paths.

use zeroclaw_api::model_provider::ConversationMessage;
use zeroclaw_providers::multimodal;

/// Whether this typed message opens a real user turn. Prompt-mode results
/// retain their internal role in replay; user-authored result-like text must
/// still open a turn, including when it carries a failed attachment.
pub fn is_turn_opening_user_message(message: &ConversationMessage) -> bool {
    matches!(
        message,
        ConversationMessage::Chat(chat)
            if chat.role == "user"
    )
}

/// Replace image references in a single message with an omission note.
///
/// The per-message form of [`degrade_media_in_messages`]: callers that carry
/// their own span bookkeeping (for example a provenance-preserving projection
/// of seed rows) apply it row by row, while span-shaped callers keep using the
/// slice form. Returns the number of image references degraded.
pub fn degrade_media_in_message(message: &mut ConversationMessage) -> usize {
    let omitted = crate::i18n::get_required_cli_string("turn-failed-attachment-omitted");
    let ConversationMessage::Chat(chat) = message else {
        return 0;
    };
    let (cleaned, refs) = multimodal::parse_image_markers(&chat.content);
    if refs.is_empty() {
        return 0;
    }
    chat.content = if cleaned.is_empty() {
        omitted
    } else {
        format!("{cleaned}\n\n{omitted}")
    };
    refs.len()
}

/// Replace image references in this message span with an omission note.
///
/// A turn that ended in a non-retryable failure may have had its attachments
/// rejected by the provider. That rejection is durable knowledge about those
/// attachments: resending them on the next request reproduces the same
/// rejection, so one bad attachment would poison every later prompt on the
/// session. The marker is replaced with a note rather than deleted so the
/// model still sees that something was attached; surrounding text survives.
///
/// Returns the number of image references degraded. Callers own the span
/// policy (which messages belong to the failed turn); this helper only
/// performs the projection.
pub fn degrade_media_in_messages(messages: &mut [ConversationMessage]) -> usize {
    messages.iter_mut().map(degrade_media_in_message).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_api::model_provider::ChatMessage;

    #[test]
    fn literal_result_prefix_opens_a_turn_but_internal_result_does_not() {
        let literal = ConversationMessage::Chat(ChatMessage::user("[Tool results]\nmy question"));
        let result = ConversationMessage::Chat(crate::agent::history::prompt_tool_results_message(
            "[Tool results]\noutput",
        ));
        assert!(is_turn_opening_user_message(&literal));
        assert!(!is_turn_opening_user_message(&result));
    }
}
