use std::fmt::Write as _;

use super::ast::PatchPath;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct AffectedPaths {
    added: Vec<PatchPath>,
    modified: Vec<PatchPath>,
    deleted: Vec<PatchPath>,
    moved: Vec<MovedPath>,
}

impl AffectedPaths {
    pub(crate) fn add_added(&mut self, path: PatchPath) {
        self.added.push(path);
    }

    pub(crate) fn add_modified(&mut self, path: PatchPath) {
        self.modified.push(path);
    }

    pub(crate) fn add_deleted(&mut self, path: PatchPath) {
        self.deleted.push(path);
    }

    pub(crate) fn add_moved(&mut self, from: PatchPath, to: PatchPath) {
        self.moved.push(MovedPath { from, to });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MovedPath {
    from: PatchPath,
    to: PatchPath,
}

pub(crate) fn format_summary(affected: &AffectedPaths) -> String {
    let mut output = String::from("Success. Updated the following files:\n");
    for path in &affected.added {
        let _ = writeln!(output, "A {}", path.raw());
    }
    for move_path in &affected.moved {
        let _ = writeln!(
            output,
            "R {} -> {}",
            move_path.from.raw(),
            move_path.to.raw()
        );
    }
    for path in &affected.modified {
        let _ = writeln!(output, "M {}", path.raw());
    }
    for path in &affected.deleted {
        let _ = writeln!(output, "D {}", path.raw());
    }
    output
}
