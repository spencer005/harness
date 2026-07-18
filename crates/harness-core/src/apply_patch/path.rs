use std::path::{Component, Path, PathBuf};

use super::{ast::PatchPath, error::ApplyPatchError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedPatchPath {
    resolved: PathBuf,
    comparison_key: PathBuf,
}

impl ResolvedPatchPath {
    pub(crate) fn resolved(&self) -> &Path {
        &self.resolved
    }

    pub(crate) fn comparison_key(&self) -> &Path {
        &self.comparison_key
    }
}

pub(crate) fn resolve_patch_path(
    cwd: &Path,
    patch_path: &PatchPath,
) -> Result<ResolvedPatchPath, ApplyPatchError> {
    validate_patch_path(patch_path)?;
    let path = patch_path.as_path();
    let resolved = cwd.join(path);
    let comparison_key = normalize_lexically(&resolved);
    Ok(ResolvedPatchPath {
        resolved,
        comparison_key,
    })
}

fn validate_patch_path(path: &PatchPath) -> Result<(), ApplyPatchError> {
    if path.raw().is_empty() {
        return Err(ApplyPatchError::unsafe_path(
            String::new(),
            "path must not be empty",
        ));
    }
    if path.raw().as_bytes().contains(&0) {
        return Err(ApplyPatchError::unsafe_path(
            path.raw().to_string(),
            "path must not contain NUL bytes",
        ));
    }
    if path.as_path().is_absolute() {
        return Err(ApplyPatchError::unsafe_path(
            path.raw().to_string(),
            "absolute paths are not allowed",
        ));
    }
    if path
        .as_path()
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ApplyPatchError::unsafe_path(
            path.raw().to_string(),
            "parent directory components are not allowed",
        ));
    }
    if !path
        .as_path()
        .components()
        .any(|component| matches!(component, Component::Normal(_)))
    {
        return Err(ApplyPatchError::unsafe_path(
            path.raw().to_string(),
            "path must name a file",
        ));
    }
    Ok(())
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.has_root() {
                    let before = normalized.clone();
                    if !normalized.pop() {
                        normalized = before;
                    }
                } else if !normalized.pop() {
                    normalized.push("..");
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_paths_against_cwd() {
        let path = PatchPath::new("nested/file.txt");
        let resolved = resolve_patch_path(Path::new("/workspace/project"), &path).unwrap();

        assert_eq!(
            resolved.resolved(),
            Path::new("/workspace/project/nested/file.txt")
        );
        assert_eq!(
            resolved.comparison_key(),
            Path::new("/workspace/project/nested/file.txt")
        );
    }

    #[test]
    fn rejects_absolute_paths() {
        let path = PatchPath::new("/tmp/apply-patch-absolute.txt");
        let error = resolve_patch_path(Path::new("/workspace"), &path).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Rejected unsafe path `/tmp/apply-patch-absolute.txt`: absolute paths are not allowed"
        );
    }

    #[test]
    fn rejects_parent_directory_components() {
        let path = PatchPath::new("../outside.txt");
        let error = resolve_patch_path(Path::new("/workspace/project"), &path).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Rejected unsafe path `../outside.txt`: parent directory components are not allowed"
        );
    }
}
