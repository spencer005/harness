use super::{
    error::ApplyPatchError,
    fs,
    fs::{PatchFileSystem, TempFile},
    plan::{PatchOperation, PatchPlan},
};

pub(crate) fn commit_plan<Fs: PatchFileSystem>(
    fs: &Fs,
    plan: &PatchPlan,
) -> Result<(), ApplyPatchError> {
    let mut staged_files = Vec::new();
    for operation in plan.operations() {
        match operation {
            PatchOperation::AddFile {
                target,
                contents,
                mode,
            } => {
                staged_files.push(StagedFile {
                    target_path: target.resolved().to_path_buf(),
                    temp: fs::stage_file_contents(fs, target.resolved(), contents, *mode)?,
                });
            }
            PatchOperation::ReplaceFile {
                target,
                replacement,
            } => {
                staged_files.push(StagedFile {
                    target_path: target.resolved().to_path_buf(),
                    temp: fs::stage_file_contents(
                        fs,
                        target.resolved(),
                        replacement.contents(),
                        replacement.mode(),
                    )?,
                });
            }
            PatchOperation::MoveFile {
                target,
                replacement: Some(replacement),
                ..
            } => {
                staged_files.push(StagedFile {
                    target_path: target.resolved().to_path_buf(),
                    temp: fs::stage_file_contents(
                        fs,
                        target.resolved(),
                        replacement.contents(),
                        replacement.mode(),
                    )?,
                });
            }
            PatchOperation::DeleteFile { .. }
            | PatchOperation::MoveFile {
                replacement: None, ..
            } => {
                if let PatchOperation::MoveFile { target, .. } = operation {
                    fs.ensure_parent_dir(target.resolved()).map_err(|source| {
                        ApplyPatchError::io(
                            format!(
                                "Failed to create parent directories for {}",
                                target.resolved().display()
                            ),
                            source,
                        )
                    })?;
                }
            }
        }
    }

    let mut staged_iter = staged_files.into_iter();
    for operation in plan.operations() {
        match operation {
            PatchOperation::AddFile { target, .. } => {
                let staged = staged_iter
                    .next()
                    .expect("add operation must have staged temp file");
                assert_eq!(staged.target_path, target.resolved());
                fs.rename_noreplace(staged.temp.path(), target.resolved())
                    .map_err(|source| {
                        remove_temp_file(&staged.temp);
                        ApplyPatchError::io(
                            format!("Failed to create file {}", target.resolved().display()),
                            source,
                        )
                    })?;
                fsync_changed_path(fs, target.resolved())?;
            }
            PatchOperation::ReplaceFile { target, .. } => {
                let staged = staged_iter
                    .next()
                    .expect("replace operation must have staged temp file");
                assert_eq!(staged.target_path, target.resolved());
                fs.rename_replace(staged.temp.path(), target.resolved())
                    .map_err(|source| {
                        remove_temp_file(&staged.temp);
                        ApplyPatchError::io(
                            format!("Failed to replace file {}", target.resolved().display()),
                            source,
                        )
                    })?;
                fsync_changed_path(fs, target.resolved())?;
            }
            PatchOperation::DeleteFile { target } => {
                fs.unlink_file_or_symlink(target.resolved())
                    .map_err(|source| {
                        ApplyPatchError::io(
                            format!("Failed to delete file {}", target.resolved().display()),
                            source,
                        )
                    })?;
                fsync_changed_path(fs, target.resolved())?;
            }
            PatchOperation::MoveFile {
                source,
                target,
                replacement: None,
            } => {
                fs.rename_noreplace(source.resolved(), target.resolved())
                    .map_err(|source_error| {
                        ApplyPatchError::io(
                            format!(
                                "Failed to move file {} to {}",
                                source.resolved().display(),
                                target.resolved().display()
                            ),
                            source_error,
                        )
                    })?;
                fsync_changed_path(fs, source.resolved())?;
                fsync_changed_path(fs, target.resolved())?;
            }
            PatchOperation::MoveFile {
                source,
                target,
                replacement: Some(_),
            } => {
                let staged = staged_iter
                    .next()
                    .expect("move update operation must have staged temp file");
                assert_eq!(staged.target_path, target.resolved());
                fs.rename_noreplace(staged.temp.path(), target.resolved())
                    .map_err(|source_error| {
                        remove_temp_file(&staged.temp);
                        ApplyPatchError::io(
                            format!("Failed to create file {}", target.resolved().display()),
                            source_error,
                        )
                    })?;
                fs.unlink_file_or_symlink(source.resolved())
                    .map_err(|source_error| {
                        ApplyPatchError::io(
                            format!("Failed to remove original {}", source.resolved().display()),
                            source_error,
                        )
                    })?;
                fsync_changed_path(fs, source.resolved())?;
                fsync_changed_path(fs, target.resolved())?;
            }
        }
    }

    Ok(())
}

#[derive(Debug)]
struct StagedFile {
    target_path: std::path::PathBuf,
    temp: TempFile,
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        remove_temp_file(&self.temp);
    }
}

fn remove_temp_file(temp: &TempFile) {
    let _ = std::fs::remove_file(temp.path());
}

fn fsync_changed_path<Fs: PatchFileSystem>(
    fs: &Fs,
    path: &std::path::Path,
) -> Result<(), ApplyPatchError> {
    match fs.fsync_parent_dir(path) {
        Ok(()) => Ok(()),
        Err(source) => Err(ApplyPatchError::io(
            format!("Failed to fsync parent directory for {}", path.display()),
            source,
        )),
    }
}
