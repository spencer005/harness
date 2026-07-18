use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PatchDocument {
    hunks: Vec<PatchHunk>,
}

impl PatchDocument {
    pub(crate) fn new(hunks: Vec<PatchHunk>) -> Self {
        Self { hunks }
    }

    pub(crate) fn hunks(&self) -> &[PatchHunk] {
        &self.hunks
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PatchHunk {
    AddFile {
        path: PatchPath,
        contents: Vec<u8>,
    },
    DeleteFile {
        path: PatchPath,
    },
    UpdateFile {
        path: PatchPath,
        move_path: Option<PatchPath>,
        chunks: Vec<UpdateChunk>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PatchPath {
    raw: String,
    path: PathBuf,
}

impl PatchPath {
    pub(crate) fn new(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let path = PathBuf::from(&raw);
        Self { raw, path }
    }

    pub(crate) fn raw(&self) -> &str {
        &self.raw
    }

    pub(crate) fn as_path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpdateChunk {
    pub(crate) change_context: Option<String>,
    pub(crate) old_lines: Vec<String>,
    pub(crate) new_lines: Vec<String>,
    pub(crate) is_end_of_file: bool,
}
