use std::io;

use thiserror::Error;

/// User-visible error returned by native `apply_patch` execution.
#[derive(Debug, Error)]
pub enum ApplyPatchError {
    #[error("Invalid patch: {0}")]
    InvalidPatch(String),
    #[error("Invalid patch hunk on line {line_number}: {message}")]
    InvalidHunk { message: String, line_number: usize },
    #[error("Rejected unsafe path `{path}`: {reason}")]
    UnsafePath { path: String, reason: &'static str },
    #[error("{0}")]
    Apply(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
}

impl ApplyPatchError {
    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    pub(crate) fn unsafe_path(path: impl Into<String>, reason: &'static str) -> Self {
        Self::UnsafePath {
            path: path.into(),
            reason,
        }
    }
}
