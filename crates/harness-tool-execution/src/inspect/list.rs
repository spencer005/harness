use std::{fs, path::Path};

use harness_tool_api::{
    InspectJobSuccess, InspectListEntry, InspectListRequest, InspectListResult, InspectPathKind,
};

use super::{InspectCommandOutput, ShellWord, resolve};

pub(crate) fn prepare(args: &[ShellWord]) -> Result<InspectListRequest, String> {
    let mut depth = 1usize;
    let mut exact = false;
    let mut limit = 500usize;
    let mut paths = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].value.as_str() {
            "--exact" => {
                if exact {
                    return Err("failed to parse `inspect` list input: duplicate `--exact`".into());
                }
                exact = true;
            }
            "--depth" | "--limit" => {
                let option = args[index].value.as_str();
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    format!("failed to parse `inspect` list input: `{option}` needs a value")
                })?;
                let parsed = super::positive(&value.value, &format!("list {option}"))?;
                if option == "--depth" {
                    depth = parsed;
                } else {
                    limit = parsed;
                }
            }
            value if value.starts_with('-') => {
                return Err(format!(
                    "failed to parse `inspect` list input: unsupported option `{value}`"
                ));
            }
            value => paths.push(value.to_owned()),
        }
        index += 1;
    }
    if paths.is_empty() {
        paths.push(".".to_owned());
    }
    Ok(InspectListRequest {
        paths,
        depth,
        exact,
        limit,
    })
}

pub(crate) fn execute(
    workspace: &super::WorkspaceRoot,
    request: &InspectListRequest,
) -> Result<InspectCommandOutput, String> {
    let mut model = String::new();
    let mut remaining = request.limit;
    let mut roots = Vec::new();
    let mut entries = Vec::new();
    for (index, requested_path) in request.paths.iter().enumerate() {
        let (name, root) = resolve(workspace, requested_path)?;
        if !root.is_dir() {
            return Err(format!("failed to list {name}: not a directory"));
        }
        roots.push(name);
        if index > 0 {
            model.push('\n');
        }
        render(
            workspace.path(),
            &root,
            Path::new(""),
            1,
            request,
            &mut remaining,
            &mut model,
            &mut entries,
        )?;
        if remaining == 0 {
            break;
        }
    }
    let truncated = remaining == 0;
    if truncated {
        model.push_str(&format!(
            "[list output truncated: showing first {} entries; use --limit or a narrower path]\n",
            request.limit
        ));
    }
    Ok(InspectCommandOutput {
        model,
        result: InspectJobSuccess::List(InspectListResult {
            roots,
            entries,
            truncated,
        }),
    })
}

#[allow(clippy::too_many_arguments)]
fn render(
    workspace: &Path,
    root: &Path,
    relative: &Path,
    current: usize,
    request: &InspectListRequest,
    remaining: &mut usize,
    model: &mut String,
    entries: &mut Vec<InspectListEntry>,
) -> Result<(), String> {
    if current > request.depth || *remaining == 0 {
        return Ok(());
    }
    let mut directory_entries = fs::read_dir(root)
        .map_err(|e| format!("failed to list {}: {e}", root.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to list {}: {e}", root.display()))?;
    directory_entries.sort_by_key(|entry| entry.file_name());
    for entry in directory_entries {
        if entry.file_name() == ".git" {
            continue;
        }
        if *remaining == 0 {
            return Ok(());
        }
        *remaining -= 1;
        let path = entry.path();
        let child = relative.join(entry.file_name());
        let metadata = fs::symlink_metadata(&path)
            .map_err(|e| format!("failed to inspect {}: {e}", path.display()))?;
        let symlink_target = metadata
            .file_type()
            .is_symlink()
            .then(|| fs::read_link(&path).ok())
            .flatten()
            .map(|target| target.display().to_string());
        let (line_count, byte_count) = if metadata.is_file() {
            let data = fs::read(&path).unwrap_or_default();
            let lines = (!data.contains(&0)).then(|| {
                data.iter().filter(|byte| **byte == b'\n').count()
                    + usize::from(!data.is_empty() && !data.ends_with(b"\n"))
            });
            (lines, Some(metadata.len()))
        } else {
            (None, None)
        };
        let kind = if metadata.is_dir() {
            InspectPathKind::Directory
        } else if metadata.file_type().is_symlink() {
            InspectPathKind::Symlink
        } else if metadata.is_file() {
            InspectPathKind::File
        } else {
            InspectPathKind::Other
        };
        let display_path = path
            .strip_prefix(workspace)
            .unwrap_or(&path)
            .display()
            .to_string();
        entries.push(InspectListEntry {
            path: display_path,
            depth: current,
            kind,
            line_count,
            byte_count,
            symlink_target: symlink_target.clone(),
        });

        let mut line = child.display().to_string();
        match kind {
            InspectPathKind::Directory => line.push('/'),
            InspectPathKind::Symlink => {
                if let Some(target) = symlink_target {
                    line.push_str(" -> ");
                    line.push_str(&target);
                }
            }
            InspectPathKind::File | InspectPathKind::Other => {}
        }
        if metadata.is_file() {
            if let Some(lines) = line_count {
                line.push_str(&format!(
                    " {lines} line{}",
                    if lines == 1 { "" } else { "s" }
                ));
                if request.exact {
                    line.push_str(&format!(" {} bytes", metadata.len()));
                }
            } else {
                line.push_str(&format!(" {}", rounded_size(metadata.len(), request.exact)));
            }
        }
        model.push_str(&line);
        model.push('\n');
        if metadata.is_dir() {
            render(
                workspace,
                &path,
                &child,
                current + 1,
                request,
                remaining,
                model,
                entries,
            )?;
        }
    }
    Ok(())
}

fn rounded_size(size: u64, exact: bool) -> String {
    if exact {
        return format!("{size} bytes");
    }
    let units = [
        (1_000_000_000_000, "TB"),
        (1_000_000_000, "GB"),
        (1_000_000, "MB"),
        (1_000, "KB"),
    ];
    for (factor, name) in units {
        if size >= factor {
            return format!("{} {name}", (size + factor / 2) / factor);
        }
    }
    format!("{size} bytes")
}
