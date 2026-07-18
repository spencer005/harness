//! Pure prompt builders for root conversation compaction.

use crate::compact::ContextWindowUsage;

const DEFAULT_COMPACTION_PROMPT: &str = "<task>Compact the conversation into a durable summary for continuing this session. The summary must include every important fact needed to resume without re-deriving intent or constraints. Preserve user goals, developer constraints, decisions, current plans, open blockers, tool outcomes, file paths, commands, diffs/patchsets, test results, retry counts, and all unresolved work. Preserve exact wording for constraints that affect code quality, safety, or task execution. Do not include, quote, paraphrase, summarize, or restate base/system/developer instructions that are supplied as request instructions; those instructions are supplied separately on continuation. Omit only transient wording and redundant chatter.</task>";

/// Build the manual compaction prompt from additional user instructions.
pub(super) fn manual_compaction_prompt(extra: &str) -> String {
    let extra = extra.trim();
    let base = DEFAULT_COMPACTION_PROMPT.to_string();
    if extra.is_empty() {
        base
    } else {
        format!("{base}\n\nAdditional compaction instruction:\n{extra}")
    }
}

/// Build the automatic compaction prompt from context-window usage.
pub(super) fn auto_compaction_prompt(usage: Option<ContextWindowUsage>) -> String {
    let base = DEFAULT_COMPACTION_PROMPT.to_string();
    format!(
        "{base}\n\nKeep the replacement summary between 4000 and 48000 words without dropping active objectives, constraints, decisions, blockers, or unresolved work."
    )
}

/// Build the prompt used after a context-window error.
pub(super) fn context_error_compaction_prompt() -> String {
    let base = DEFAULT_COMPACTION_PROMPT;
    format!(
        "{base}\n\nThe previous request exceeded the context window. Produce a tighter summary that keeps all durable state needed to continue, including active objectives, constraints, decisions, blockers, tool outcomes, and unresolved work."
    )
}

/// Build the system instructions used by the compaction request.
pub(super) fn compaction_instructions(base_instructions: &str) -> String {
    base_instructions.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_prompt_excludes_request_instructions_from_summary() {
        assert!(DEFAULT_COMPACTION_PROMPT.contains(
            "Do not include, quote, paraphrase, summarize, or restate base/system/developer instructions that are supplied as request instructions"
        ));
        assert!(
            DEFAULT_COMPACTION_PROMPT
                .contains("those instructions are supplied separately on continuation")
        );
    }

    #[test]
    fn compaction_instructions_keep_only_base_prefix() {
        let instructions = compaction_instructions("Base instructions.");

        assert_eq!(instructions, "Base instructions.");
    }
}
