//! Pure construction of the built-in slash-command catalog.

use crate::commands::{CommandCatalog, CommandDefinition, CommandRegistrar};

/// Built-in slash command actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SlashCommand {
    /// Toggle developer-role input mode.
    Developer,
    /// Update model settings.
    Model,
    /// Show provider profile status.
    Provider,
    /// Override native tool wire format.
    ToolsOverride,
    /// Toggle PTY terminal tools for the model.
    Terminal,
    /// Start session compaction.
    Compact,
    /// Roll back to a persisted session sequence and fork a new session from there.
    Rollback,
    /// Continue root work across response boundaries.
    Persist,
}

/// Build the built-in slash-command catalog.
pub(super) fn slash_command_catalog() -> CommandCatalog<SlashCommand> {
    let mut registrar = CommandRegistrar::new();
    register_slash_command(
        &mut registrar,
        "/developer",
        "Toggle developer-role input mode.",
        "/developer [on|off]",
        SlashCommand::Developer,
        &[],
    );
    register_slash_command(
        &mut registrar,
        "/model",
        "Set the model, reasoning effort, and service tier for future requests.",
        "/model <model> [reasoning] [tier]",
        SlashCommand::Model,
        &[],
    );
    register_slash_command(
        &mut registrar,
        "/provider",
        "Show the current provider profile.",
        "/provider",
        SlashCommand::Provider,
        &[],
    );
    register_slash_command(
        &mut registrar,
        "/toolsoverride",
        "Override native tool wire format for the current session.",
        "/toolsoverride [custom|compat]",
        SlashCommand::ToolsOverride,
        &[],
    );
    register_slash_command(
        &mut registrar,
        "/terminal",
        "Toggle PTY terminal tools for the model.",
        "/terminal [on|off]",
        SlashCommand::Terminal,
        &[],
    );
    register_slash_command(
        &mut registrar,
        "/compact",
        "Compact the current session context.",
        "/compact [instruction]",
        SlashCommand::Compact,
        &[],
    );
    register_slash_command(
        &mut registrar,
        "/rollback",
        "Fork a new session from the current session at an inclusive persisted sequence number.",
        "/rollback <seq>",
        SlashCommand::Rollback,
        &[],
    );
    register_slash_command(
        &mut registrar,
        "/persist",
        "Toggle automatic continuation of the current or explicit task until the model verifies completion and marks it complete.",
        "/persist [task|pause|continue]",
        SlashCommand::Persist,
        &[],
    );
    registrar.build()
}

fn register_slash_command(
    registrar: &mut CommandRegistrar<SlashCommand>,
    name: &'static str,
    description: &'static str,
    usage: &'static str,
    action: SlashCommand,
    aliases: &[&'static str],
) {
    let mut definition = CommandDefinition::new(name, description, usage, action)
        .expect("built-in slash command names must be valid");
    for alias in aliases {
        definition = definition
            .with_alias(*alias)
            .expect("built-in slash command aliases must be valid");
    }
    registrar
        .register(definition)
        .expect("built-in slash command lookups must be unique");
}
