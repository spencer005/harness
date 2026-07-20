use std::{env, fs};

use super::ShellWord;
pub(crate) fn execute(args: &[ShellWord]) -> Result<String, String> {
    if args.len() != 1 {
        return Err("failed to parse `inspect` input: usage: `which <query>`".into());
    }
    let query = args[0].value.to_ascii_lowercase();
    let path = env::var_os("PATH").ok_or("failed to search commands: PATH is not set")?;
    let mut out = String::new();
    let mut seen = std::collections::BTreeSet::new();
    for directory in env::split_paths(&path) {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() || !is_executable(&meta) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.to_ascii_lowercase().contains(&query) && seen.insert(name.clone()) {
                out.push_str(&format!("{name} {}\n", entry.path().display()));
            }
        }
    }
    if out.is_empty() {
        out.push_str("no results\n");
    }
    Ok(out)
}
#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}
#[cfg(not(unix))]
fn is_executable(_: &fs::Metadata) -> bool {
    true
}
