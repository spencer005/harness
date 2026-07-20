use std::process::Command;

use super::{ShellWord, WorkspaceRoot};
pub(crate) fn check(workspace: &WorkspaceRoot, args: &[ShellWord]) -> Result<String, String> {
    run(workspace, "check", args)
}
pub(crate) fn test(workspace: &WorkspaceRoot, args: &[ShellWord]) -> Result<String, String> {
    run(workspace, "test", args)
}
fn run(workspace: &WorkspaceRoot, command: &str, args: &[ShellWord]) -> Result<String, String> {
    let values = args
        .iter()
        .map(|arg| arg.value.as_str())
        .collect::<Vec<_>>();
    let output = Command::new("cargo")
        .arg(command)
        .arg("--locked")
        .args(values)
        .current_dir(workspace.path())
        .output()
        .map_err(|e| format!("failed to execute `cargo {command}`: {e}"))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if output.status.success() {
        Ok(text)
    } else {
        Err(if text.is_empty() {
            format!("cargo {command} failed\n")
        } else {
            text
        })
    }
}
