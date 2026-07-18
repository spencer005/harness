//! Pure compaction result normalization helpers.

use crate::compact::CompactResult;

/// Build a compact result from raw model text.
pub(super) fn compact_result_from_text(text: String) -> CompactResult {
    CompactResult::new(normalized_compaction_summary(&text), Vec::new())
}

/// Normalize a compaction summary into non-empty durable text.
pub(super) fn normalized_compaction_summary(summary: &str) -> String {
    let summary = summary.trim();
    if summary.is_empty() {
        "No durable context retained.".to_string()
    } else {
        summary.to_string()
    }
}

/// Build the model-visible checkpoint text for a compaction summary.
pub(super) fn compaction_checkpoint_text(summary: &str) -> String {
    format!("Conversation summary after compaction:\n\n{summary}")
}
