use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use super::{
    ast::{PatchDocument, PatchHunk, PatchPath, UpdateChunk},
    error::ApplyPatchError,
    fs,
    fs::PatchFileSystem,
    matchers::derive_updated_file,
    path::{ResolvedPatchPath, resolve_patch_path},
    report::AffectedPaths,
};

#[derive(Debug, Clone)]
pub(crate) struct PatchPlan {
    operations: Vec<PatchOperation>,
    affected: AffectedPaths,
}

impl PatchPlan {
    pub(crate) fn operations(&self) -> &[PatchOperation] {
        &self.operations
    }

    pub(crate) fn affected(&self) -> &AffectedPaths {
        &self.affected
    }
}

#[derive(Debug, Clone)]
pub(crate) enum PatchOperation {
    AddFile {
        target: ResolvedPatchPath,
        contents: Vec<u8>,
        mode: u32,
    },
    DeleteFile {
        target: ResolvedPatchPath,
    },
    ReplaceFile {
        target: ResolvedPatchPath,
        replacement: ReplacementFile,
    },
    MoveFile {
        source: ResolvedPatchPath,
        target: ResolvedPatchPath,
        replacement: Option<ReplacementFile>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ReplacementFile {
    contents: Vec<u8>,
    mode: u32,
}

impl ReplacementFile {
    pub(crate) fn contents(&self) -> &[u8] {
        &self.contents
    }

    pub(crate) fn mode(&self) -> u32 {
        self.mode
    }
}

pub(crate) fn build_plan<Fs: PatchFileSystem>(
    fs: &Fs,
    cwd: &Path,
    document: &PatchDocument,
) -> Result<PatchPlan, ApplyPatchError> {
    if document.hunks().is_empty() {
        return Err(ApplyPatchError::Apply(
            "No files were modified.".to_string(),
        ));
    }

    let mut exclusive_source_paths = HashSet::new();
    let mut destination_paths = HashSet::new();
    let mut pending_update_indices = HashMap::new();
    let mut pending_updates = Vec::new();
    let mut operations = Vec::new();
    let mut affected = AffectedPaths::default();

    for hunk in document.hunks() {
        match hunk {
            PatchHunk::AddFile { path, contents } => {
                let target = resolve(cwd, path)?;
                reject_duplicate_destination(&mut destination_paths, &target)?;
                reject_existing_destination(fs, &target)?;
                affected.add_added(path.clone());
                operations.push(PatchOperation::AddFile {
                    target,
                    contents: contents.clone(),
                    mode: 0o644,
                });
            }
            PatchHunk::DeleteFile { path } => {
                let target = resolve(cwd, path)?;
                reject_duplicate_source(
                    &mut exclusive_source_paths,
                    &pending_update_indices,
                    &target,
                )?;
                reject_duplicate_destination(&mut destination_paths, &target)?;
                fs::ensure_file_or_symlink_for_delete(fs, target.resolved())?;
                affected.add_deleted(path.clone());
                operations.push(PatchOperation::DeleteFile { target });
            }
            PatchHunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => {
                let source = resolve(cwd, path)?;
                reject_exclusive_source_conflict(&exclusive_source_paths, &source)?;
                let move_target = if let Some(move_path) = move_path {
                    let target = resolve(cwd, move_path)?;
                    reject_same_path_move(&source, &target)?;
                    Some((move_path.clone(), target))
                } else {
                    None
                };
                push_pending_update(
                    &mut pending_updates,
                    &mut pending_update_indices,
                    path,
                    source,
                    move_target,
                    chunks,
                )?;
            }
        }
    }

    for update in pending_updates {
        let original_file = fs::read_regular_file(fs, update.source.resolved())?;
        let replacement = if update.chunks.is_empty() {
            None
        } else {
            Some(ReplacementFile {
                contents: derive_updated_file(
                    update.source.resolved(),
                    original_file.contents(),
                    &update.chunks,
                )?,
                mode: original_file.metadata().mode(),
            })
        };

        if let Some(target) = update.move_target {
            let move_path = update
                .move_path
                .expect("move target must retain its original patch path");
            reject_duplicate_destination(&mut destination_paths, &target)?;
            reject_existing_destination(fs, &target)?;
            affected.add_moved(update.source_path, move_path);
            operations.push(PatchOperation::MoveFile {
                source: update.source,
                target,
                replacement,
            });
        } else {
            reject_duplicate_destination(&mut destination_paths, &update.source)?;
            affected.add_modified(update.source_path);
            operations.push(PatchOperation::ReplaceFile {
                target: update.source,
                replacement: replacement.expect("update without move must have chunks"),
            });
        }
    }

    Ok(PatchPlan {
        operations,
        affected,
    })
}

#[derive(Debug, Clone)]
struct PendingUpdate {
    source_path: PatchPath,
    source: ResolvedPatchPath,
    move_path: Option<PatchPath>,
    move_target: Option<ResolvedPatchPath>,
    chunks: Vec<UpdateChunk>,
}

fn push_pending_update(
    pending_updates: &mut Vec<PendingUpdate>,
    pending_update_indices: &mut HashMap<PathBuf, usize>,
    path: &PatchPath,
    source: ResolvedPatchPath,
    move_target: Option<(PatchPath, ResolvedPatchPath)>,
    chunks: &[UpdateChunk],
) -> Result<(), ApplyPatchError> {
    let source_key = source.comparison_key().to_path_buf();
    if let Some(index) = pending_update_indices.get(&source_key).copied() {
        let update = &mut pending_updates[index];
        if let Some((move_path, target)) = move_target {
            merge_move_target(update, move_path, target)?;
        }
        update.chunks.extend_from_slice(chunks);
        return Ok(());
    }

    let (move_path, move_target) = if let Some((move_path, target)) = move_target {
        (Some(move_path), Some(target))
    } else {
        (None, None)
    };
    pending_update_indices.insert(source_key, pending_updates.len());
    pending_updates.push(PendingUpdate {
        source_path: path.clone(),
        source,
        move_path,
        move_target,
        chunks: chunks.to_vec(),
    });
    Ok(())
}

fn merge_move_target(
    update: &mut PendingUpdate,
    move_path: PatchPath,
    target: ResolvedPatchPath,
) -> Result<(), ApplyPatchError> {
    if let Some(existing_target) = &update.move_target {
        if existing_target.comparison_key() != target.comparison_key() {
            return Err(ApplyPatchError::Apply(format!(
                "Patch moves source path {} to multiple destinations",
                update.source.resolved().display()
            )));
        }
    } else {
        update.move_path = Some(move_path);
        update.move_target = Some(target);
    }
    Ok(())
}

fn reject_exclusive_source_conflict(
    exclusive_sources: &HashSet<PathBuf>,
    path: &ResolvedPatchPath,
) -> Result<(), ApplyPatchError> {
    if exclusive_sources.contains(path.comparison_key()) {
        return Err(ApplyPatchError::Apply(format!(
            "Patch references source path {} more than once",
            path.resolved().display()
        )));
    }
    Ok(())
}

fn resolve(cwd: &Path, path: &PatchPath) -> Result<ResolvedPatchPath, ApplyPatchError> {
    resolve_patch_path(cwd, path)
}

fn reject_duplicate_source(
    exclusive_sources: &mut HashSet<PathBuf>,
    pending_update_indices: &HashMap<PathBuf, usize>,
    path: &ResolvedPatchPath,
) -> Result<(), ApplyPatchError> {
    if pending_update_indices.contains_key(path.comparison_key())
        || !exclusive_sources.insert(path.comparison_key().to_path_buf())
    {
        return Err(ApplyPatchError::Apply(format!(
            "Patch references source path {} more than once",
            path.resolved().display()
        )));
    }
    Ok(())
}

fn reject_duplicate_destination(
    seen: &mut HashSet<PathBuf>,
    path: &ResolvedPatchPath,
) -> Result<(), ApplyPatchError> {
    if !seen.insert(path.comparison_key().to_path_buf()) {
        return Err(ApplyPatchError::Apply(format!(
            "Patch writes destination path {} more than once",
            path.resolved().display()
        )));
    }
    Ok(())
}

fn reject_same_path_move(
    source: &ResolvedPatchPath,
    target: &ResolvedPatchPath,
) -> Result<(), ApplyPatchError> {
    if source.comparison_key() == target.comparison_key() {
        return Err(ApplyPatchError::Apply(format!(
            "Refusing to move {} onto itself",
            source.resolved().display()
        )));
    }
    Ok(())
}

fn reject_existing_destination<Fs: PatchFileSystem>(
    fs: &Fs,
    target: &ResolvedPatchPath,
) -> Result<(), ApplyPatchError> {
    if fs::path_exists(fs, target.resolved())? {
        return Err(ApplyPatchError::Apply(format!(
            "Refusing to overwrite existing file {}",
            target.resolved().display()
        )));
    }
    Ok(())
}
