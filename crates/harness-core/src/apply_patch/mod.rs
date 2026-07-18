mod ast;
mod error;
mod fs;
mod matchers;
mod path;
mod plan;
mod report;
mod syntax;
mod txn;

use std::path::Path;

pub use error::ApplyPatchError;
use fs::UnixFileSystem;
use plan::build_plan;
use report::format_summary;
use syntax::parse_patch;
use txn::commit_plan;

/// Parsed patch preview produced without mutating the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchPreview {
    /// Summary that would be returned after a successful apply.
    pub summary: String,
}

/// Parse and apply a Codex freeform `apply_patch` payload relative to `cwd`.
///
/// Patch paths must be relative to `cwd`; absolute paths and parent-directory
/// components are rejected. The function derives all file contents before
/// mutating the filesystem and stages writes through near-target temporary files.
pub fn apply_patch(cwd: impl AsRef<Path>, patch: &str) -> Result<String, ApplyPatchError> {
    let document = parse_patch(patch)?;
    let fs = UnixFileSystem;
    let plan = build_plan(&fs, cwd.as_ref(), &document)?;
    commit_plan(&fs, &plan)?;
    Ok(format_summary(plan.affected()))
}

/// Parse and validate a Codex freeform `apply_patch` payload without mutation.
///
/// This uses the same parser and planner as [`apply_patch`], including all
/// path, context, no-clobber, and destination checks that can be performed
/// before filesystem mutation.
pub fn preview_patch(cwd: impl AsRef<Path>, patch: &str) -> Result<PatchPreview, ApplyPatchError> {
    let document = parse_patch(patch)?;
    let fs = UnixFileSystem;
    let plan = build_plan(&fs, cwd.as_ref(), &document)?;
    Ok(PatchPreview {
        summary: format_summary(plan.affected()),
    })
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    static TEMP_ORDINAL: AtomicU64 = AtomicU64::new(0);

    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new(label: &str) -> Self {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after Unix epoch")
                .as_nanos();
            let ordinal = TEMP_ORDINAL.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "harness-core-apply-patch-{label}-{}-{now}-{ordinal}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create temp root");
            Self { path }
        }

        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn applies_add_update_and_delete_hunks() {
        let root = TempRoot::new("multi");
        fs::write(root.path().join("modify.txt"), "line1\nline2\nline3\n").unwrap();
        fs::write(root.path().join("delete.txt"), "obsolete\n").unwrap();

        let output = apply_patch(
            root.path(),
            "*** Begin Patch\n*** Add File: nested/new.txt\n+created\n*** Update File: modify.txt\n@@\n-line2\n+changed\n*** Delete File: delete.txt\n*** End Patch",
        )
        .unwrap();

        assert_eq!(
            output,
            "Success. Updated the following files:\nA nested/new.txt\nM modify.txt\nD delete.txt\n"
        );
        assert_eq!(
            fs::read(root.path().join("nested/new.txt")).unwrap(),
            b"created\n"
        );
        assert_eq!(
            fs::read(root.path().join("modify.txt")).unwrap(),
            b"line1\nchanged\nline3\n"
        );
        assert!(!root.path().join("delete.txt").exists());
    }

    #[test]
    fn moves_file_to_new_directory() {
        let root = TempRoot::new("move");
        let original = root.path().join("old/name.txt");
        fs::create_dir_all(original.parent().unwrap()).unwrap();
        fs::write(&original, "old content\n").unwrap();

        let output = apply_patch(
            root.path(),
            "*** Begin Patch\n*** Update File: old/name.txt\n*** Move to: renamed/dir/name.txt\n@@\n-old content\n+new content\n*** End Patch",
        )
        .unwrap();

        assert_eq!(
            output,
            "Success. Updated the following files:\nR old/name.txt -> renamed/dir/name.txt\n"
        );
        assert!(!original.exists());
        assert_eq!(
            fs::read(root.path().join("renamed/dir/name.txt")).unwrap(),
            b"new content\n"
        );
    }

    #[test]
    fn applies_multiple_chunks_and_end_of_file_marker() {
        let root = TempRoot::new("chunks");
        fs::write(
            root.path().join("multi.txt"),
            "first\nsecond\nthird\nfourth\n",
        )
        .unwrap();

        let output = apply_patch(
            root.path(),
            "*** Begin Patch\n*** Update File: multi.txt\n@@\n-second\n+changed second\n@@\n third\n-fourth\n+changed fourth\n*** End of File\n*** End Patch",
        )
        .unwrap();

        assert_eq!(
            output,
            "Success. Updated the following files:\nM multi.txt\n"
        );
        assert_eq!(
            fs::read(root.path().join("multi.txt")).unwrap(),
            b"first\nchanged second\nthird\nchanged fourth\n"
        );
    }

    #[test]
    fn applies_repeated_update_file_sections_as_one_file_update() {
        let root = TempRoot::new("repeated-update");
        fs::write(root.path().join("same.txt"), "alpha\nbeta\ngamma\ndelta\n").unwrap();

        let output = apply_patch(
            root.path(),
            "*** Begin Patch\n*** Update File: same.txt\n@@\n-beta\n+changed beta\n*** Update File: same.txt\n@@\n-delta\n+changed delta\n*** End Patch",
        )
        .unwrap();

        assert_eq!(
            output,
            "Success. Updated the following files:\nM same.txt\n"
        );
        assert_eq!(
            fs::read(root.path().join("same.txt")).unwrap(),
            b"alpha\nchanged beta\ngamma\nchanged delta\n"
        );
    }

    #[test]
    fn preview_patch_validates_without_mutating_file() {
        let root = TempRoot::new("preview");
        let target = root.path().join("preview.txt");
        fs::write(&target, "before\n").unwrap();

        let preview = preview_patch(
            root.path(),
            "*** Begin Patch\n*** Update File: preview.txt\n@@\n-before\n+after\n*** End Patch",
        )
        .unwrap();

        assert_eq!(
            preview.summary,
            "Success. Updated the following files:\nM preview.txt\n"
        );
        assert_eq!(fs::read(target).unwrap(), b"before\n");
    }

    #[test]
    fn merges_repeated_update_sections_for_moved_file() {
        let root = TempRoot::new("repeated-move-update");
        fs::write(root.path().join("old.txt"), "one\ntwo\nthree\n").unwrap();

        let output = apply_patch(
            root.path(),
            "*** Begin Patch\n*** Update File: old.txt\n*** Move to: new.txt\n@@\n-one\n+changed one\n*** Update File: old.txt\n@@\n-three\n+changed three\n*** End Patch",
        )
        .unwrap();

        assert_eq!(
            output,
            "Success. Updated the following files:\nR old.txt -> new.txt\n"
        );
        assert!(!root.path().join("old.txt").exists());
        assert_eq!(
            fs::read(root.path().join("new.txt")).unwrap(),
            b"changed one\ntwo\nchanged three\n"
        );
    }

    #[test]
    fn rejects_missing_context_without_mutating_file() {
        let root = TempRoot::new("missing-context");
        let target = root.path().join("modify.txt");
        fs::write(&target, "line1\nline2\n").unwrap();

        let error = apply_patch(
            root.path(),
            "*** Begin Patch\n*** Update File: modify.txt\n@@\n-missing\n+changed\n*** End Patch",
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            format!(
                "Failed to find expected lines in {}:\nmissing",
                target.display()
            )
        );
        assert_eq!(fs::read(target).unwrap(), b"line1\nline2\n");
    }

    #[test]
    fn rejects_invalid_update_hunk() {
        let root = TempRoot::new("invalid-hunk");

        let error = apply_patch(
            root.path(),
            "*** Begin Patch\n*** Update File: foo.txt\n*** End Patch",
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "Invalid patch hunk on line 2: Update file hunk for path 'foo.txt' is empty"
        );
    }

    #[test]
    fn rejects_absolute_paths_and_parent_components_before_mutation() {
        let root = TempRoot::new("confined-paths");
        let absolute_path = root.path().join("absolute.txt");

        let error = apply_patch(
            root.path(),
            &format!(
                "*** Begin Patch\n*** Add File: {}\n+absolute\n*** End Patch",
                absolute_path.display()
            ),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "Rejected unsafe path `{}`: absolute paths are not allowed",
                absolute_path.display()
            )
        );
        assert!(!absolute_path.exists());

        let error = apply_patch(
            root.path(),
            "*** Begin Patch\n*** Add File: nested/../parent.txt\n+parent\n*** End Patch",
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Rejected unsafe path `nested/../parent.txt`: parent directory components are not allowed"
        );
        assert!(!root.path().join("parent.txt").exists());
    }

    #[test]
    fn rejects_nul_and_empty_paths_before_any_mutation() {
        let root = TempRoot::new("unsafe");
        let error = apply_patch(
            root.path(),
            "*** Begin Patch\n*** Add File: safe.txt\n+safe\n*** Delete File: \n*** End Patch",
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "Rejected unsafe path ``: path must not be empty"
        );
        assert!(!root.path().join("safe.txt").exists());

        let error = apply_patch(
            root.path(),
            "*** Begin Patch\n*** Add File: bad\0path\n+bad\n*** End Patch",
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "Rejected unsafe path `bad\0path`: path must not contain NUL bytes"
        );
    }

    #[test]
    fn add_file_is_no_clobber() {
        let root = TempRoot::new("add-clobber");
        let target = root.path().join("exists.txt");
        fs::write(&target, "original\n").unwrap();

        let error = apply_patch(
            root.path(),
            "*** Begin Patch\n*** Add File: exists.txt\n+new\n*** End Patch",
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            format!("Refusing to overwrite existing file {}", target.display())
        );
        assert_eq!(fs::read(target).unwrap(), b"original\n");
    }

    #[test]
    fn move_destination_is_no_clobber() {
        let root = TempRoot::new("move-clobber");
        fs::write(root.path().join("source.txt"), "source\n").unwrap();
        let dest = root.path().join("dest.txt");
        fs::write(&dest, "dest\n").unwrap();

        let error = apply_patch(
            root.path(),
            "*** Begin Patch\n*** Update File: source.txt\n*** Move to: dest.txt\n*** End Patch",
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            format!("Refusing to overwrite existing file {}", dest.display())
        );
        assert_eq!(
            fs::read(root.path().join("source.txt")).unwrap(),
            b"source\n"
        );
        assert_eq!(fs::read(dest).unwrap(), b"dest\n");
    }

    #[test]
    fn pure_move_renames_without_requiring_chunks() {
        let root = TempRoot::new("pure-move");
        let original = root.path().join("old.txt");
        fs::write(&original, "content\n").unwrap();

        let output = apply_patch(
            root.path(),
            "*** Begin Patch\n*** Update File: old.txt\n*** Move to: new.txt\n*** End Patch",
        )
        .unwrap();

        assert_eq!(
            output,
            "Success. Updated the following files:\nR old.txt -> new.txt\n"
        );
        assert!(!original.exists());
        assert_eq!(fs::read(root.path().join("new.txt")).unwrap(), b"content\n");
    }

    #[test]
    fn pure_move_creates_destination_parent_directories() {
        let root = TempRoot::new("pure-move-parent");
        let original = root.path().join("old.txt");
        fs::write(&original, "content\n").unwrap();

        apply_patch(
            root.path(),
            "*** Begin Patch\n*** Update File: old.txt\n*** Move to: nested/dir/new.txt\n*** End Patch",
        )
        .unwrap();

        assert!(!original.exists());
        assert_eq!(
            fs::read(root.path().join("nested/dir/new.txt")).unwrap(),
            b"content\n"
        );
    }

    #[test]
    fn same_path_move_is_rejected() {
        let root = TempRoot::new("same-move");
        let original = root.path().join("same.txt");
        fs::write(&original, "content\n").unwrap();

        let error = apply_patch(
            root.path(),
            "*** Begin Patch\n*** Update File: same.txt\n*** Move to: ./same.txt\n*** End Patch",
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            format!("Refusing to move {} onto itself", original.display())
        );
        assert_eq!(fs::read(original).unwrap(), b"content\n");
    }

    #[test]
    fn delete_symlink_removes_link_not_target() {
        let root = TempRoot::new("delete-symlink");
        let target = root.path().join("target.txt");
        let link = root.path().join("link.txt");
        fs::write(&target, "target\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        apply_patch(
            root.path(),
            "*** Begin Patch\n*** Delete File: link.txt\n*** End Patch",
        )
        .unwrap();

        assert_eq!(fs::read(target).unwrap(), b"target\n");
        assert!(!link.exists());
    }
}
