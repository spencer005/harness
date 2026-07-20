use std::process::Command;

use super::ShellWord;
pub(crate) fn execute(args: &[ShellWord]) -> Result<String, String> {
    if args.len() > 1 {
        return Err("failed to parse `inspect` input: usage: `ps [name]`".into());
    }
    let output = Command::new("ps")
        .args(["aux"])
        .output()
        .map_err(|e| format!("failed to execute `ps`: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    if let Some(filter) = args.first() {
        let filter = filter.value.to_ascii_lowercase();
        let mut lines = text.lines();
        let header = lines.next().unwrap_or_default();
        let matches = lines
            .filter(|line| line.to_ascii_lowercase().contains(&filter))
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return Ok("no results\n".into());
        }
        return Ok(format!("{}\n{}\n", header, matches.join("\n")));
    }
    Ok(format!("{}\n", text.trim_end()))
}
