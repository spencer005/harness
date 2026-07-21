use std::{fs, path::Path};

use super::{ShellWord, resolve};

pub(crate) fn execute(
    workspace: &super::WorkspaceRoot,
    args: &[ShellWord],
) -> Result<String, String> {
    let mut depth = 1usize;
    let mut exact = false;
    let mut limit = 500usize;
    let mut paths: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].value.as_str() {
            "--exact" => {
                if exact {
                    return Err("failed to parse `inspect` list input: duplicate `--exact`".into());
                }
                exact = true;
                index += 1;
            }
            "--depth" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or("failed to parse `inspect` list input: `--depth` needs a value")?;
                depth = super::positive(&value.value, "list --depth")?;
                index += 1;
            }
            "--limit" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or("failed to parse `inspect` list input: `--limit` needs a value")?;
                limit = super::positive(&value.value, "list --limit")?;
                index += 1;
            }
            value if value.starts_with('-') => {
                index += 1;
            }
            value => {
                paths.push(value.to_owned());
                index += 1;
            }
        }
    }
    if paths.is_empty() {
        paths.push(".".to_owned());
    }
    let mut output = String::new();
    let mut remaining = limit;
    for (index, path) in paths.iter().enumerate() {
        let (name, root) = resolve(workspace, path)?;
        if !root.is_dir() {
            return Err(format!("failed to list {name}: not a directory"));
        }
        if index > 0 {
            output.push('\n');
        }
        render(
            &root,
            Path::new(""),
            1,
            depth,
            exact,
            &mut remaining,
            &mut output,
        )?;
        if remaining == 0 {
            break;
        }
    }
    if remaining == 0 {
        output.push_str(&format!(
            "[list output truncated: showing first {limit} entries; use --limit or a narrower path]\n",
        ));
    }
    Ok(output)
}

fn render(
    root: &Path,
    relative: &Path,
    current: usize,
    maximum: usize,
    exact: bool,
    remaining: &mut usize,
    output: &mut String,
) -> Result<(), String> {
    if current > maximum || *remaining == 0 {
        return Ok(());
    }
    let mut entries = fs::read_dir(root)
        .map_err(|e| format!("failed to list {}: {e}", root.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to list {}: {e}", root.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if *remaining == 0 {
            return Ok(());
        }
        *remaining -= 1;
        let path = entry.path();
        let child = relative.join(entry.file_name());
        let metadata = fs::symlink_metadata(&path)
            .map_err(|e| format!("failed to inspect {}: {e}", path.display()))?;
        let mut line = child.display().to_string();
        if metadata.is_dir() {
            line.push('/');
        } else if metadata.file_type().is_symlink() {
            if let Ok(target) = fs::read_link(&path) {
                line.push_str(" -> ");
                line.push_str(&target.display().to_string());
            }
        }
        if metadata.is_file() {
            let data = fs::read(&path).unwrap_or_default();
            if data.contains(&0) {
                line.push_str(&format!(" {}", rounded_size(metadata.len(), exact)));
            } else {
                let lines = data.iter().filter(|byte| **byte == b'\n').count()
                    + usize::from(!data.is_empty() && !data.ends_with(b"\n"));
                line.push_str(&format!(
                    " {} line{}",
                    lines,
                    if lines == 1 { "" } else { "s" }
                ));
                if exact {
                    line.push_str(&format!(" {} bytes", metadata.len()));
                }
            }
        }
        output.push_str(&line);
        output.push('\n');
        if metadata.is_dir() {
            render(
                &path,
                &child,
                current + 1,
                maximum,
                exact,
                remaining,
                output,
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