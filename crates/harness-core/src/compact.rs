use serde::{Deserialize, Serialize};
use sonic_rs::Value;
use thiserror::Error;
use tiktoken_rs::o200k_base_singleton;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Request sent to a compaction model.
pub struct CompactRequest {
    /// Prompt instructing the model how to compact the history.
    pub prompt: String,
    /// History items serialized in Responses input-item shape.
    pub history_items_json: Vec<Value>,
}

impl CompactRequest {
    /// Create a compaction request from a prompt and history items.
    pub fn new(prompt: impl Into<String>, history_items_json: Vec<Value>) -> Self {
        Self {
            prompt: prompt.into(),
            history_items_json,
        }
    }

    /// Estimate total input tokens for prompt plus history items.
    pub fn estimated_input_tokens(&self) -> u64 {
        estimate_text_tokens(&self.prompt) + estimate_history_items_tokens(&self.history_items_json)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Result returned by a successful compaction request.
pub struct CompactResult {
    /// Durable compacted summary text.
    pub summary: String,
    /// Replacement history items that remain after compaction.
    pub replacement_history_json: Vec<Value>,
}

impl CompactResult {
    /// Create a compaction result from a summary and replacement history.
    pub fn new(summary: impl Into<String>, replacement_history_json: Vec<Value>) -> Self {
        Self {
            summary: summary.into(),
            replacement_history_json,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Token thresholds that decide when history compaction is needed.
pub struct ContextWindowPolicy {
    /// Model context-window input token limit.
    pub max_input_tokens: u64,
    /// Estimated token count at which compaction starts.
    pub compact_at_tokens: u64,
    /// Target estimated token count after compaction.
    pub target_tokens_after_compaction: u64,
}

impl ContextWindowPolicy {
    /// Validate and create a context-window policy.
    pub fn new(
        max_input_tokens: u64,
        compact_at_tokens: u64,
        target_tokens_after_compaction: u64,
    ) -> Result<Self, CompactPlanError> {
        if max_input_tokens == 0 {
            return Err(CompactPlanError::EmptyContextWindow);
        }
        if compact_at_tokens == 0 {
            return Err(CompactPlanError::EmptyCompactionThreshold);
        }
        if compact_at_tokens > max_input_tokens {
            return Err(CompactPlanError::ThresholdExceedsWindow {
                compact_at_tokens,
                max_input_tokens,
            });
        }
        if target_tokens_after_compaction >= compact_at_tokens {
            return Err(CompactPlanError::TargetNotBelowThreshold {
                target_tokens_after_compaction,
                compact_at_tokens,
            });
        }
        Ok(Self {
            max_input_tokens,
            compact_at_tokens,
            target_tokens_after_compaction,
        })
    }

    /// Decide whether an estimated token count requires compaction.
    pub fn assess_tokens(&self, estimated_input_tokens: u64) -> ContextWindowDecision {
        let usage = ContextWindowUsage {
            estimated_input_tokens,
            max_input_tokens: self.max_input_tokens,
            compact_at_tokens: self.compact_at_tokens,
            target_tokens_after_compaction: self.target_tokens_after_compaction,
        };
        if estimated_input_tokens >= self.compact_at_tokens {
            ContextWindowDecision::Compact(usage)
        } else {
            ContextWindowDecision::Continue(usage)
        }
    }

    /// Decide whether a request requires compaction.
    pub fn assess_request(&self, request: &CompactRequest) -> ContextWindowDecision {
        self.assess_tokens(request.estimated_input_tokens())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Token usage compared against a context-window policy.
pub struct ContextWindowUsage {
    /// Estimated current request input tokens.
    pub estimated_input_tokens: u64,
    /// Model context-window input token limit.
    pub max_input_tokens: u64,
    /// Threshold that triggers compaction.
    pub compact_at_tokens: u64,
    /// Target estimated token count after compaction.
    pub target_tokens_after_compaction: u64,
}

impl ContextWindowUsage {
    /// Return whether usage exceeds the absolute model window.
    pub fn exceeds_window(&self) -> bool {
        self.estimated_input_tokens > self.max_input_tokens
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Decision returned by context-window assessment.
pub enum ContextWindowDecision {
    /// Request may continue without compaction.
    Continue(ContextWindowUsage),
    /// Request should compact before continuing.
    Compact(ContextWindowUsage),
}

impl ContextWindowDecision {
    /// Return the usage values associated with the decision.
    pub fn usage(&self) -> ContextWindowUsage {
        match self {
            Self::Continue(usage) | Self::Compact(usage) => *usage,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Reason a compaction plan is requested.
pub enum CompactionTrigger {
    /// Estimated usage crossed the configured compaction threshold.
    ContextWindowPressure,
    /// The upstream model reported a context-window error.
    ContextWindowError,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Concrete compaction execution plan.
pub enum CompactionPlan {
    /// Run one compaction request.
    Single(CompactRequest),
    /// Run two chronological compaction requests.
    Parallel(TwoBlockCompactionPlan),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Two chronological compaction requests split from one oversized history.
pub struct TwoBlockCompactionPlan {
    /// Request compacting the older chronological block.
    pub older: CompactRequest,
    /// Request compacting the newer chronological block.
    pub newer: CompactRequest,
}

impl TwoBlockCompactionPlan {
    /// Return the two requests in chronological order.
    pub fn requests(&self) -> [&CompactRequest; 2] {
        [&self.older, &self.newer]
    }

    /// Merge chronological block results into one compaction result.
    pub fn merge_results(&self, older: CompactResult, newer: CompactResult) -> CompactResult {
        let mut replacement_history_json = older.replacement_history_json;
        replacement_history_json.extend(newer.replacement_history_json);
        CompactResult {
            summary: format!(
                "chronological block 1 of 2:\n{}\n\nchronological block 2 of 2:\n{}",
                older.summary.trim(),
                newer.summary.trim()
            ),
            replacement_history_json,
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
/// Error returned while validating or creating a compaction plan.
pub enum CompactPlanError {
    /// Context-window size is zero.
    #[error("context window max input tokens must be greater than zero")]
    EmptyContextWindow,
    /// Compaction threshold is zero.
    #[error("context window compaction threshold must be greater than zero")]
    EmptyCompactionThreshold,
    /// Compaction threshold exceeds the model context window.
    #[error("compaction threshold {compact_at_tokens} exceeds context window {max_input_tokens}")]
    ThresholdExceedsWindow {
        /// Configured compaction threshold.
        compact_at_tokens: u64,
        /// Configured context-window size.
        max_input_tokens: u64,
    },
    /// Compaction target is not below the compaction threshold.
    #[error(
        "compaction target {target_tokens_after_compaction} must be below threshold {compact_at_tokens}"
    )]
    TargetNotBelowThreshold {
        /// Configured post-compaction target.
        target_tokens_after_compaction: u64,
        /// Configured compaction threshold.
        compact_at_tokens: u64,
    },
    /// Split compaction does not have enough history items.
    #[error("split compaction requires at least two history items, got {history_items}")]
    NotEnoughHistoryToSplit {
        /// Number of history items in the request.
        history_items: usize,
    },
}

/// Create a compaction execution plan for a trigger and request.
pub fn plan_compaction(
    trigger: CompactionTrigger,
    request: CompactRequest,
) -> Result<CompactionPlan, CompactPlanError> {
    match trigger {
        CompactionTrigger::ContextWindowPressure => Ok(CompactionPlan::Single(request)),
        CompactionTrigger::ContextWindowError => {
            split_context_error_compaction(request).map(CompactionPlan::Parallel)
        }
    }
}

/// Split one context-error compaction request into two chronological blocks.
pub fn split_context_error_compaction(
    request: CompactRequest,
) -> Result<TwoBlockCompactionPlan, CompactPlanError> {
    let history_len = request.history_items_json.len();
    if history_len < 2 {
        return Err(CompactPlanError::NotEnoughHistoryToSplit {
            history_items: history_len,
        });
    }

    let split_index = split_index_by_estimated_tokens(&request.history_items_json);
    let prompt = request.prompt;
    let mut older_items = request.history_items_json;
    let newer_items = older_items.split_off(split_index);

    Ok(TwoBlockCompactionPlan {
        older: CompactRequest::new(
            block_prompt(&prompt, ParallelCompactionBlock::Older),
            older_items,
        ),
        newer: CompactRequest::new(
            block_prompt(&prompt, ParallelCompactionBlock::Newer),
            newer_items,
        ),
    })
}

/// Estimate token count for serialized history items.
pub fn estimate_history_items_tokens(history_items_json: &[Value]) -> u64 {
    history_items_json
        .iter()
        .map(estimate_json_value_tokens)
        .sum()
}

/// Estimate token count for one JSON value.
pub fn estimate_json_value_tokens(value: &Value) -> u64 {
    estimate_text_tokens(&value.to_string())
}

/// Count text tokens with the OpenAI `o200k_base` tokenizer.
pub fn estimate_text_tokens(text: &str) -> u64 {
    u64::try_from(o200k_base_singleton().count_ordinary(text)).expect("token count fits in u64")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParallelCompactionBlock {
    Older,
    Newer,
}

fn split_index_by_estimated_tokens(history_items_json: &[Value]) -> usize {
    let total_tokens = estimate_history_items_tokens(history_items_json);
    let midpoint = total_tokens.div_ceil(2);
    let mut running_tokens = 0;

    for (index, item) in history_items_json
        .iter()
        .enumerate()
        .take(history_items_json.len() - 1)
    {
        running_tokens += estimate_json_value_tokens(item);
        if running_tokens >= midpoint {
            return index + 1;
        }
    }

    history_items_json.len() / 2
}

fn block_prompt(prompt: &str, block: ParallelCompactionBlock) -> String {
    match block {
        ParallelCompactionBlock::Older => format!(
            "{prompt}\n\nCompact chronological block 1 of 2. This block contains the older history items. Preserve durable facts, user/developer constraints, unresolved tasks, and tool outcomes without assuming access to block 2."
        ),
        ParallelCompactionBlock::Newer => format!(
            "{prompt}\n\nCompact chronological block 2 of 2. This block contains the newer history items. Preserve durable facts, user/developer constraints, unresolved tasks, and tool outcomes without assuming access to block 1."
        ),
    }
}

#[cfg(test)]
mod tests {
    use sonic_rs::json;

    use super::*;

    #[test]
    fn context_window_policy_compacts_at_threshold() {
        let policy = ContextWindowPolicy::new(100, 80, 50).unwrap();

        assert!(matches!(
            policy.assess_tokens(79),
            ContextWindowDecision::Continue(ContextWindowUsage {
                estimated_input_tokens: 79,
                ..
            })
        ));

        let decision = policy.assess_tokens(80);
        assert!(matches!(decision, ContextWindowDecision::Compact(_)));
        let usage = decision.usage();
        assert_eq!(usage.estimated_input_tokens, 80);
        assert_eq!(usage.max_input_tokens, 100);
        assert!(!usage.exceeds_window());
    }

    #[test]
    fn context_window_policy_rejects_invalid_windows() {
        assert_eq!(
            ContextWindowPolicy::new(0, 1, 0),
            Err(CompactPlanError::EmptyContextWindow)
        );
        assert_eq!(
            ContextWindowPolicy::new(10, 0, 0),
            Err(CompactPlanError::EmptyCompactionThreshold)
        );
        assert_eq!(
            ContextWindowPolicy::new(10, 11, 5),
            Err(CompactPlanError::ThresholdExceedsWindow {
                compact_at_tokens: 11,
                max_input_tokens: 10
            })
        );
        assert_eq!(
            ContextWindowPolicy::new(10, 8, 8),
            Err(CompactPlanError::TargetNotBelowThreshold {
                target_tokens_after_compaction: 8,
                compact_at_tokens: 8
            })
        );
    }

    #[test]
    fn estimate_text_tokens_uses_o200k_base() {
        assert_eq!(estimate_text_tokens("hello world"), 2);
        assert_eq!(estimate_text_tokens("お誕生日おめでとう"), 8);
    }

    #[test]
    fn context_error_compaction_splits_into_two_parallel_blocks() {
        let history = vec![
            json!({"role": "user", "content": "short"}),
            json!({"role": "assistant", "content": "short"}),
            json!({"role": "user", "content": "this item is deliberately much longer than the others so the split is token based"}),
            json!({"role": "assistant", "content": "tail"}),
        ];

        let plan = split_context_error_compaction(CompactRequest::new(
            "Summarize this conversation.",
            history.clone(),
        ))
        .unwrap();

        assert!(!plan.older.history_items_json.is_empty());
        assert!(!plan.newer.history_items_json.is_empty());
        assert_eq!(
            plan.older.history_items_json.len() + plan.newer.history_items_json.len(),
            history.len()
        );

        let mut planned_history = plan.older.history_items_json.clone();
        planned_history.extend(plan.newer.history_items_json.clone());
        assert_eq!(planned_history, history);
    }

    #[test]
    fn compaction_policy_uses_single_request_for_pressure_and_parallel_for_error() {
        let pressure_request = CompactRequest::new("compact", vec![json!({"a": 1})]);
        assert!(matches!(
            plan_compaction(
                CompactionTrigger::ContextWindowPressure,
                pressure_request.clone()
            ),
            Ok(CompactionPlan::Single(request)) if request == pressure_request
        ));

        let error_request = CompactRequest::new("compact", vec![json!({"a": 1}), json!({"b": 2})]);
        assert!(matches!(
            plan_compaction(CompactionTrigger::ContextWindowError, error_request),
            Ok(CompactionPlan::Parallel(_))
        ));
    }

    #[test]
    fn context_error_compaction_requires_two_history_items() {
        assert_eq!(
            split_context_error_compaction(CompactRequest::new("compact", vec![json!({"a": 1})])),
            Err(CompactPlanError::NotEnoughHistoryToSplit { history_items: 1 })
        );
    }

    #[test]
    fn parallel_compaction_results_merge_in_chronological_order() {
        let plan = split_context_error_compaction(CompactRequest::new(
            "compact",
            vec![json!({"turn": 1}), json!({"turn": 2})],
        ))
        .unwrap();

        let merged = plan.merge_results(
            CompactResult::new("older summary", vec![json!({"summary": "older"})]),
            CompactResult::new("newer summary", vec![json!({"summary": "newer"})]),
        );

        assert_eq!(
            merged.summary,
            "chronological block 1 of 2:\nolder summary\n\nchronological block 2 of 2:\nnewer summary"
        );
        assert_eq!(
            merged.replacement_history_json,
            vec![json!({"summary": "older"}), json!({"summary": "newer"})]
        );
    }
}
