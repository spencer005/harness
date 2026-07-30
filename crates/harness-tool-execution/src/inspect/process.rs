use std::process::Command;

use harness_tool_api::{InspectJobSuccess, InspectProcess, InspectPsRequest, InspectPsResult};

use super::{InspectCommandOutput, ShellWord};

pub(crate) fn prepare(args: &[ShellWord]) -> Result<InspectPsRequest, String> {
    if args.len() > 1 {
        return Err("failed to parse `inspect` input: usage: `ps [name]`".into());
    }
    Ok(InspectPsRequest {
        filter: args.first().map(|word| word.value.clone()),
    })
}

pub(crate) fn execute(request: &InspectPsRequest) -> Result<InspectCommandOutput, String> {
    let output = Command::new("ps")
        .args(["aux"])
        .output()
        .map_err(|e| format!("failed to execute `ps`: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "failed to execute `ps`: exited with {}",
            output
                .status
                .code()
                .map_or_else(|| "signal".to_owned(), |code| code.to_string())
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let header = lines.next().unwrap_or_default();
    let filter = request
        .filter
        .as_ref()
        .map(|filter| filter.to_ascii_lowercase());
    let selected = lines
        .filter(|line| {
            filter
                .as_ref()
                .is_none_or(|filter| line.to_ascii_lowercase().contains(filter))
        })
        .collect::<Vec<_>>();
    let model = if selected.is_empty() {
        "no results\n".to_owned()
    } else {
        format!("{header}\n{}\n", selected.join("\n"))
    };
    let processes = selected
        .into_iter()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() < 11 {
                return None;
            }
            Some(InspectProcess {
                user: fields[0].to_owned(),
                pid: fields[1].parse().ok()?,
                cpu_percent: fields[2].to_owned(),
                memory_percent: fields[3].to_owned(),
                command: fields[10..].join(" "),
            })
        })
        .collect();
    Ok(InspectCommandOutput {
        model,
        result: InspectJobSuccess::Ps(InspectPsResult { processes }),
    })
}
