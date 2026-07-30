use std::{env, fs};

use harness_tool_api::{
    InspectCommandMatch, InspectJobSuccess, InspectWhichRequest, InspectWhichResult,
};

use super::{InspectCommandOutput, ShellWord};

pub(crate) fn prepare(args: &[ShellWord]) -> Result<InspectWhichRequest, String> {
    if args.len() != 1 {
        return Err("failed to parse `inspect` input: usage: `which <query>`".into());
    }
    Ok(InspectWhichRequest {
        query: args[0].value.clone(),
    })
}

pub(crate) fn execute(request: &InspectWhichRequest) -> Result<InspectCommandOutput, String> {
    let query = request.query.to_ascii_lowercase();
    let path = env::var_os("PATH").ok_or("failed to search commands: PATH is not set")?;
    let mut matches = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for directory in env::split_paths(&path) {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() || !is_executable(&meta) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.to_ascii_lowercase().contains(&query) && seen.insert(name.clone()) {
                matches.push(InspectCommandMatch {
                    name,
                    path: entry.path().display().to_string(),
                });
            }
        }
    }
    let mut model = String::new();
    for found in &matches {
        model.push_str(&format!("{} {}\n", found.name, found.path));
    }
    if matches.is_empty() {
        model.push_str("no results\n");
    }
    Ok(InspectCommandOutput {
        model,
        result: InspectJobSuccess::Which(InspectWhichResult { matches }),
    })
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
