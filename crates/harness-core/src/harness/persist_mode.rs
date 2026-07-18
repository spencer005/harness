//! Pure persist-mode command decisions.

/// Action selected for a `/persist` slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PersistCommandAction {
    /// Enable persist mode using the previous task in session history.
    EnablePreviousTask,
    /// Enable persist mode after recording an explicit task.
    EnableExplicitTask(String),
    /// Disable active persist mode.
    Disable,
    /// Continue a paused persist mode.
    Continue,
    /// Pause an active persist mode.
    Pause,
}

/// Decide how a `/persist` slash command updates persist mode.
pub(super) fn persist_command_action(
    root_persist_active: bool,
    task: &str,
) -> PersistCommandAction {
    let task = task.trim();
    if task == "continue" {
        PersistCommandAction::Continue
    } else if task == "pause" {
        PersistCommandAction::Pause
    } else if root_persist_active && task.is_empty() {
        PersistCommandAction::Disable
    } else if task.is_empty() {
        PersistCommandAction::EnablePreviousTask
    } else {
        PersistCommandAction::EnableExplicitTask(task.to_string())
    }
}
