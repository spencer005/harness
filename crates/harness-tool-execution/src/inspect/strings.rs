use std::fs;

use harness_tool_api::{
    InspectJobSuccess, InspectStringMatch, InspectStringsRequest, InspectStringsResult,
};

use super::{InspectCommandOutput, ShellWord, resolve};

pub(crate) fn prepare(args: &[ShellWord]) -> Result<InspectStringsRequest, String> {
    let mut positional = Vec::new();
    let mut maximum_results = 100usize;
    let mut index = 0;
    while index < args.len() {
        match args[index].value.as_str() {
            "--max" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or("failed to parse `inspect` input: `--max` needs a value")?;
                maximum_results = super::positive(&value.value, "strings --max")?;
            }
            value if value.starts_with('-') => {
                return Err(format!(
                    "failed to parse `inspect` strings input: unsupported option `{value}`"
                ));
            }
            value => positional.push(value.to_owned()),
        }
        index += 1;
    }
    if positional.is_empty() || positional.len() > 2 {
        return Err("failed to parse `inspect` input: usage: `strings <path> [literal]`".into());
    }
    Ok(InspectStringsRequest {
        path: positional.remove(0),
        literal: positional.pop(),
        maximum_results,
    })
}

pub(crate) fn execute(
    workspace: &super::WorkspaceRoot,
    request: &InspectStringsRequest,
) -> Result<InspectCommandOutput, String> {
    let (name, path) = resolve(workspace, &request.path)?;
    let data = fs::read(&path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let mut matches = Vec::new();
    let mut run = Vec::new();
    let mut offset = 0usize;
    let mut total_matches = 0;
    for (index, byte) in data
        .iter()
        .enumerate()
        .chain(std::iter::once((data.len(), &0)))
    {
        if byte.is_ascii_graphic() || *byte == b' ' {
            if run.is_empty() {
                offset = index;
            }
            run.push(*byte);
        } else {
            if run.len() >= 4 {
                let text = String::from_utf8_lossy(&run);
                if request
                    .literal
                    .as_deref()
                    .is_none_or(|value| text.contains(value))
                {
                    if matches.len() < request.maximum_results {
                        matches.push(InspectStringMatch {
                            offset: offset as u64,
                            text: text.into_owned(),
                        });
                    }
                    total_matches += 1;
                }
            }
            run.clear();
        }
    }
    let mut model = String::new();
    for found in &matches {
        model.push_str(&format!("{} {}\n", found.offset, found.text));
    }
    if total_matches == 0 {
        model.push_str("no results\n");
    } else if total_matches > matches.len() {
        model.push_str(&format!(
            "[strings output truncated: showing first {} results; use --max]\n",
            request.maximum_results
        ));
    }
    Ok(InspectCommandOutput {
        model,
        result: InspectJobSuccess::Strings(InspectStringsResult {
            path: name,
            matches,
            total_matches,
        }),
    })
}
