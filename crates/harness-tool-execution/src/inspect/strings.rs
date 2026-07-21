use std::fs;
use super::{ShellWord, resolve};
pub(crate) fn execute(workspace: &super::WorkspaceRoot, args: &[ShellWord]) -> Result<String, String> {
    let mut path_arg: Option<String> = None;
    let mut literal: Option<String> = None;
    let mut max = 100usize;
    let mut index = 0;
    while index < args.len() {
        match args[index].value.as_str() {
            "--max" => {
                index += 1;
                let value = args.get(index).ok_or(
                    "failed to parse `inspect` input: `--max` needs a value",
                )?;
                max = super::positive(&value.value, "strings --max")?;
                index += 1;
            }
            value if value.starts_with('-') => {
                index += 1;
            }
            value => {
                if path_arg.is_none() {
                    path_arg = Some(value.to_owned());
                } else if literal.is_none() {
                    literal = Some(value.to_owned());
                }
                index += 1;
            }
        }
    }
    let path = path_arg.ok_or("failed to parse `inspect` input: usage: `strings <path> [literal]`")?;
    let (_, path) = resolve(workspace, &path)?;
    let data = fs::read(&path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let literal = literal.as_deref();
    let mut out = String::new();
    let mut run = Vec::new();
    let mut offset = 0usize;
    let mut matches = 0;
    for (index, byte) in data.iter().enumerate().chain(std::iter::once((data.len(), &0))) {
        if byte.is_ascii_graphic() || *byte == b' ' {
            if run.is_empty() {
                offset = index;
            }
            run.push(*byte);
        } else {
            if run.len() >= 4 {
                let text = String::from_utf8_lossy(&run);
                if literal.is_none_or(|value| text.contains(value)) {
                    if matches < max {
                        out.push_str(&format!("{offset} {text}\n"));
                    }
                    matches += 1;
                }
            }
            run.clear();
        }
    }
    if matches == 0 {
        out.push_str("no results\n");
    } else if matches > max {
        out.push_str(&format!("[strings output truncated: showing first {max} results; use --max]\n"));
    }
    Ok(out)
}
