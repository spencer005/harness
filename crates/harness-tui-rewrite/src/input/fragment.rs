//! Resource-bound typestate transition for exact prompt fragments.

use std::marker::PhantomData;

use super::PromptCapacityError;

/// Exact prompt text received from a terminal input event.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RawInput;

/// Prompt text proven to fit standalone editor resource limits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BoundedInput;

/// Prompt fragment parameterized by its resource-validation stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InputFragment<State> {
    text: String,
    _state: PhantomData<State>,
}

impl InputFragment<RawInput> {
    /// Wraps exact user-owned prompt text without normalization.
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            _state: PhantomData,
        }
    }

    /// Proves that this fragment independently fits the prompt storage limit.
    pub(crate) fn bound(self) -> Result<InputFragment<BoundedInput>, PromptCapacityError> {
        validate_prompt_size(&self.text)?;
        Ok(InputFragment {
            text: self.text,
            _state: PhantomData,
        })
    }
}

impl InputFragment<BoundedInput> {
    /// Returns exact resource-bounded prompt text.
    pub(crate) fn as_str(&self) -> &str {
        &self.text
    }

    /// Consumes the fragment and returns exact prompt text.
    pub(crate) fn into_string(self) -> String {
        self.text
    }
}

pub(super) fn validate_prompt_size(text: &str) -> Result<(), PromptCapacityError> {
    let actual_bytes = text.len();
    if actual_bytes > super::MAX_PROMPT_BYTES {
        return Err(PromptCapacityError {
            actual_bytes,
            maximum_bytes: super::MAX_PROMPT_BYTES,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_fragment_preserves_controls_and_line_endings_exactly() {
        let source = "one\r\n\u{1b}[31m\t\u{202e}";
        let fragment = InputFragment::<RawInput>::new(source).bound().unwrap();

        assert_eq!(fragment.as_str(), source);
    }

    #[test]
    fn resource_transition_rejects_oversized_source() {
        let error = InputFragment::<RawInput>::new("x".repeat(super::super::MAX_PROMPT_BYTES + 1))
            .bound()
            .unwrap_err();

        assert_eq!(error.actual_bytes(), super::super::MAX_PROMPT_BYTES + 1);
    }
}
