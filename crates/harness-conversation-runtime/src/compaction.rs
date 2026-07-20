//! Compaction staging and result validation.
//!
//! Compaction is transactional: a model result is staged against an immutable
//! source snapshot and cannot replace active history until it passes validation
//! and the replacement checkpoint is durable.

use harness_session_store::SessionPayload;
use thiserror::Error;

/// Immutable source captured when compaction begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionSource {
    /// Canonical revision represented by `history`.
    pub revision: u64,
    /// History that must remain active until commit succeeds.
    pub history: Vec<SessionPayload>,
}

/// Output accumulated from one compaction model response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionDraft {
    /// Source snapshot summarized by this draft.
    pub source: CompactionSource,
    /// User or automatic instructions used for this run.
    pub instruction: String,
    /// Text received from the model.
    pub summary: String,
}

impl CompactionDraft {
    /// Creates an empty draft for one source snapshot.
    pub fn new(source: CompactionSource, instruction: String) -> Self {
        Self {
            source,
            instruction,
            summary: String::new(),
        }
    }

    /// Adds one streamed model fragment without normalizing it prematurely.
    pub fn push_delta(&mut self, delta: &str) {
        self.summary.push_str(delta);
    }

    /// Validates the complete response and returns the normalized summary.
    pub fn finish(self) -> Result<ValidatedCompaction, CompactionValidationError> {
        validate_summary(&self.summary).map(|summary| ValidatedCompaction {
            source: self.source,
            instruction: self.instruction,
            summary,
        })
    }
}

/// A compaction result safe to stage for durable commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCompaction {
    /// Source snapshot that must be replaced.
    pub source: CompactionSource,
    /// Instruction used for this run.
    pub instruction: String,
    /// Trimmed, nontrivial summary.
    pub summary: String,
}

/// Validates streamed compaction output without pretending to assess semantic completeness.
pub fn validate_summary(summary: &str) -> Result<String, CompactionValidationError> {
    let normalized = summary.trim();
    if normalized.is_empty() {
        return Err(CompactionValidationError::Empty);
    }
    if normalized.chars().count() < 32 {
        return Err(CompactionValidationError::TooShort {
            characters: normalized.chars().count(),
        });
    }
    if looks_truncated(normalized) {
        return Err(CompactionValidationError::LooksTruncated);
    }
    Ok(normalized.to_owned())
}

fn looks_truncated(summary: &str) -> bool {
    let last = summary.chars().last();
    matches!(last, Some(':') | Some(',') | Some(';') | Some('-'))
        || summary.ends_with("...")
        || summary.ends_with('\u{2026}')
}

/// Coordinates one staged compaction and its preserved source snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionCoordinator {
    state: CompactionState,
}

impl CompactionCoordinator {
    /// Begins compaction against an immutable source snapshot.
    pub fn begin(
        source: CompactionSource,
        instruction: String,
    ) -> Result<Self, CompactionStateError> {
        Ok(Self {
            state: CompactionState::Running {
                draft: CompactionDraft::new(source, instruction),
            },
        })
    }

    /// Adds streamed output to the active compaction.
    pub fn push_delta(&mut self, delta: &str) -> Result<(), CompactionStateError> {
        match &mut self.state {
            CompactionState::Running { draft } => {
                draft.push_delta(delta);
                Ok(())
            }
            CompactionState::Failed { .. } | CompactionState::Validated { .. } => {
                Err(CompactionStateError::NeedsRedo)
            }
            CompactionState::Committed | CompactionState::Cancelled => {
                Err(CompactionStateError::NotActive)
            }
        }
    }

    /// Finishes the model response and stages a validated result.
    pub fn finish(&mut self) -> Result<&ValidatedCompaction, CompactionValidationError> {
        let state = std::mem::replace(&mut self.state, CompactionState::Cancelled);
        match state {
            CompactionState::Running { draft } => {
                let source = draft.source.clone();
                let instruction = draft.instruction.clone();
                match draft.finish() {
                    Ok(result) => {
                        self.state = CompactionState::Validated { result };
                        self.validated()
                    }
                    Err(error) => {
                        self.state = CompactionState::Failed { source, instruction };
                        Err(error)
                    }
                }
            }
            other => {
                self.state = other;
                Err(CompactionValidationError::NotRunning)
            }
        }
    }

    /// Returns the validated result ready for durable commit.
    pub fn validated(&self) -> Result<&ValidatedCompaction, CompactionValidationError> {
        match &self.state {
            CompactionState::Validated { result } => Ok(result),
            CompactionState::Failed { .. } => Err(CompactionValidationError::NeedsRedo),
            CompactionState::Running { .. } => Err(CompactionValidationError::NotFinished),
            CompactionState::Committed | CompactionState::Cancelled => {
                Err(CompactionValidationError::NotRunning)
            }
        }
    }

    /// Marks the validated result as durably committed.
    pub fn commit(&mut self) -> Result<ValidatedCompaction, CompactionStateError> {
        let state = std::mem::replace(&mut self.state, CompactionState::Cancelled);
        match state {
            CompactionState::Validated { result } => {
                self.state = CompactionState::Committed;
                Ok(result)
            }
            other => {
                self.state = other;
                Err(CompactionStateError::NotValidated)
            }
        }
    }

    /// Restarts compaction from the preserved pre-compaction source.
    pub fn redo(&mut self) -> Result<(), CompactionStateError> {
        self.redo_with_instruction(None)
    }

    /// Restarts compaction from the preserved source with optional new instructions.
    pub fn redo_with_instruction(
        &mut self,
        instruction: Option<String>,
    ) -> Result<(), CompactionStateError> {
        let (source, previous_instruction) = match &self.state {
            CompactionState::Failed { source, instruction } => {
                (source.clone(), instruction.clone())
            }
            CompactionState::Validated { result } => {
                (result.source.clone(), result.instruction.clone())
            }
            CompactionState::Running { draft } => {
                (draft.source.clone(), draft.instruction.clone())
            }
            CompactionState::Committed | CompactionState::Cancelled => {
                return Err(CompactionStateError::SourceUnavailable)
            }
        };
        self.state = CompactionState::Running {
            draft: CompactionDraft::new(
                source,
                instruction.unwrap_or(previous_instruction),
            ),
        };
        Ok(())
    }

    /// Cancels the staged operation without modifying the source history.
    pub fn cancel(&mut self) {
        self.state = CompactionState::Cancelled;
    }

    /// Returns the validated summary ready for durable commit.
    pub fn validated_summary(&self) -> Option<&str> {
        match &self.state {
            CompactionState::Validated { result } => Some(&result.summary),
            _ => None,
        }
    }

    /// Returns the instruction used by the active or staged compaction.
    pub fn instruction(&self) -> Option<&str> {
        match &self.state {
            CompactionState::Running { draft } => Some(&draft.instruction),
            CompactionState::Failed { instruction, .. } => Some(instruction),
            CompactionState::Validated { result } => Some(&result.instruction),
            CompactionState::Committed | CompactionState::Cancelled => None,
        }
    }

    /// Returns the preserved source while redo remains possible.
    pub fn source(&self) -> Option<&CompactionSource> {
        match &self.state {
            CompactionState::Running { draft } => Some(&draft.source),
            CompactionState::Failed { source, .. } => Some(source),
            CompactionState::Validated { result } => Some(&result.source),
            CompactionState::Committed | CompactionState::Cancelled => None,
        }
    }
}

/// State of a compaction transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CompactionState {
    Running { draft: CompactionDraft },
    Failed { source: CompactionSource, instruction: String },
    Validated { result: ValidatedCompaction },
    Committed,
    Cancelled,
}

/// Invalid operation on the compaction transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CompactionStateError {
    /// Another compaction is already receiving model output.
    #[error("compaction is already running")]
    AlreadyRunning,
    /// The operation requires a failed or validated source.
    #[error("the original compaction source is unavailable")]
    SourceUnavailable,
    /// The operation requires a validated result.
    #[error("compaction has not produced a validated result")]
    NotValidated,
    /// The operation requires an active compaction.
    #[error("compaction is not active")]
    NotActive,
    /// The result must be redone before more output can be accepted.
    #[error("compaction result requires redo")]
    NeedsRedo,
}

/// Failure produced before a compaction can be committed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CompactionValidationError {
    /// The model returned no usable text.
    #[error("compaction returned no summary")]
    Empty,
    /// The model returned too little text to safely replace history.
    #[error("compaction summary is too short ({characters} characters)")]
    TooShort { characters: usize },
    /// The response has a common incomplete-output shape.
    #[error("compaction summary appears truncated")]
    LooksTruncated,
    /// The model response has not ended yet.
    #[error("compaction response has not finished")]
    NotFinished,
    /// A previous response failed validation and must be redone.
    #[error("compaction result requires redo")]
    NeedsRedo,
    /// No active or staged compaction exists.
    #[error("compaction is not active")]
    NotRunning,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_short_results_are_rejected() {
        assert_eq!(validate_summary(" \n"), Err(CompactionValidationError::Empty));
        assert!(matches!(
            validate_summary("too short"),
            Err(CompactionValidationError::TooShort { .. })
        ));
    }

    #[test]
    fn obvious_partial_sentence_is_rejected() {
        assert_eq!(
            validate_summary("The active task and unresolved blockers are:"),
            Err(CompactionValidationError::LooksTruncated)
        );
    }

    #[test]
    fn valid_summary_is_trimmed_only_at_the_boundary() {
        let input = "  User goal: port the tool runtime. Preserve the test constraints.  ";
        assert_eq!(
            validate_summary(input).unwrap(),
            "User goal: port the tool runtime. Preserve the test constraints."
        );
    }

    #[test]
    fn failed_compaction_can_be_redone_from_the_original_source() {
        let source = CompactionSource {
            revision: 7,
            history: Vec::new(),
        };
        let mut coordinator = CompactionCoordinator::begin(source.clone(), "retain blockers".into())
            .unwrap();
        coordinator
            .push_delta("The active task and unresolved blockers are:")
            .unwrap();
        assert_eq!(
            coordinator.finish(),
            Err(CompactionValidationError::LooksTruncated)
        );
        assert_eq!(coordinator.source(), Some(&source));
        coordinator
            .redo_with_instruction(Some("also preserve test failures".into()))
            .unwrap();
        coordinator
            .push_delta("Goal: preserve the active implementation plan and unresolved blockers.")
            .unwrap();
        coordinator.finish().unwrap();
        let committed = coordinator.commit().unwrap();
        assert_eq!(committed.source.revision, 7);
    }

    #[test]
    fn cancel_discards_staged_compaction_without_touching_source() {
        let source = CompactionSource {
            revision: 3,
            history: Vec::new(),
        };
        let mut coordinator = CompactionCoordinator::begin(source, String::new()).unwrap();
        coordinator.cancel();
        assert_eq!(coordinator.source(), None);
        assert_eq!(coordinator.redo(), Err(CompactionStateError::SourceUnavailable));
    }
}
