use std::{fs, path::PathBuf};

use super::{ShellWord, resolve};

pub(crate) fn execute(
    workspace: &super::WorkspaceRoot,
    args: &[ShellWord],
) -> Result<String, String> {
    if args.is_empty() {
        return Err("failed to parse `inspect` search input: pattern is required".into());
    }
    let pattern = &args[0].value;
    let root = args
        .get(1)
        .map(|arg| resolve(workspace, &arg.value))
        .transpose()?;
    let base = root
        .as_ref()
        .map(|(_, path)| path.as_path())
        .unwrap_or(workspace.path());
    let mut files = Vec::new();
    collect(base, &mut files)?;
    files.sort();
    let mut output = String::new();
    let mut count = 0;
    for file in files {
        let Ok(data) = fs::read(&file) else { continue };
        let Ok(text) = String::from_utf8(data) else {
            continue;
        };
        for (line, value) in text.lines().enumerate() {
            if value.contains(pattern) {
                let relative = file.strip_prefix(workspace.path()).unwrap_or(&file);
                output.push_str(&format!("{}:{}:{}\n", relative.display(), line + 1, value));
                count += 1;
                if count == 100 {
                    output.push_str("search output truncated\n");
                    return Ok(output);
                }
            }
        }
    }
    if count == 0 {
        output.push_str("no results\n");
    }
    Ok(output)
}
fn collect(path: &std::path::Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| format!("failed to inspect {}: {e}", path.display()))?;
    if metadata.is_file() {
        files.push(path.to_owned());
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    for entry in
        fs::read_dir(path).map_err(|e| format!("failed to list {}: {e}", path.display()))?
    {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_name() != ".git" {
            collect(&entry.path(), files)?;
        }
    }
    Ok(())
}
