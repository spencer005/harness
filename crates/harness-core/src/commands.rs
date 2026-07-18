use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use thiserror::Error;

/// Validated slash-command name used for registration and lookup.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommandName(String);

impl CommandName {
    /// Creates a command name from a slash-prefixed identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, CommandNameError> {
        let name = value.into();
        validate_command_name(&name)?;
        Ok(Self(name))
    }

    /// Returns the slash-prefixed command name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for CommandName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for CommandName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<&str> for CommandName {
    type Error = CommandNameError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// Error returned when a command name does not match the registry grammar.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("invalid command name `{name}`: {reason}")]
pub struct CommandNameError {
    name: String,
    reason: &'static str,
}

impl CommandNameError {
    /// Returns the invalid command name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the validation failure reason.
    pub fn reason(&self) -> &'static str {
        self.reason
    }
}

/// Command definition accepted by a registrar.
#[derive(Debug, Clone)]
pub struct CommandDefinition<TAction> {
    name: CommandName,
    description: String,
    usage: String,
    aliases: Vec<CommandName>,
    action: TAction,
}

impl<TAction> CommandDefinition<TAction> {
    /// Creates a command definition with a validated command name.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        usage: impl Into<String>,
        action: TAction,
    ) -> Result<Self, CommandNameError> {
        Ok(Self {
            name: CommandName::new(name)?,
            description: description.into(),
            usage: usage.into(),
            aliases: Vec::new(),
            action,
        })
    }

    /// Adds a validated alias to the command definition.
    pub fn with_alias(mut self, alias: impl Into<String>) -> Result<Self, CommandNameError> {
        self.aliases.push(CommandName::new(alias)?);
        Ok(self)
    }
}

/// Registered command metadata and executable action.
#[derive(Debug, Clone)]
pub struct RegisteredCommand<TAction> {
    name: CommandName,
    description: String,
    usage: String,
    aliases: Vec<CommandName>,
    action: TAction,
}

impl<TAction> RegisteredCommand<TAction> {
    fn from_definition(definition: CommandDefinition<TAction>) -> Self {
        Self {
            name: definition.name,
            description: definition.description,
            usage: definition.usage,
            aliases: definition.aliases,
            action: definition.action,
        }
    }

    /// Returns the canonical command name.
    pub fn name(&self) -> &CommandName {
        &self.name
    }

    /// Returns the human-readable command description.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Returns the command usage string.
    pub fn usage(&self) -> &str {
        &self.usage
    }

    /// Returns registered aliases for this command.
    pub fn aliases(&self) -> &[CommandName] {
        &self.aliases
    }

    /// Returns the action associated with this command.
    pub fn action(&self) -> &TAction {
        &self.action
    }
}

/// Error returned while adding a command to a registrar.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CommandRegistrationError {
    /// The command definition has an empty description.
    #[error("command `{command}` must include a non-empty description")]
    EmptyDescription {
        /// Command with the empty description.
        command: CommandName,
    },
    /// The command definition has an empty usage string.
    #[error("command `{command}` must include a non-empty usage string")]
    EmptyUsage {
        /// Command with the empty usage string.
        command: CommandName,
    },
    /// A name or alias is already registered.
    #[error(
        "command lookup `{lookup}` for `{command}` conflicts with registered command `{registered_command}`"
    )]
    DuplicateLookup {
        /// Name or alias that conflicts.
        lookup: CommandName,
        /// Command currently being registered.
        command: CommandName,
        /// Existing command that owns the lookup.
        registered_command: CommandName,
    },
}

/// Mutable command registrar used during runtime startup.
#[derive(Debug)]
pub struct CommandRegistrar<TAction> {
    commands: Vec<RegisteredCommand<TAction>>,
    lookup: HashMap<CommandName, usize>,
}

impl<TAction> CommandRegistrar<TAction> {
    /// Creates an empty registrar.
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
            lookup: HashMap::new(),
        }
    }

    /// Registers a command definition and reserves all of its lookups.
    pub fn register(
        &mut self,
        definition: CommandDefinition<TAction>,
    ) -> Result<(), CommandRegistrationError> {
        let command = RegisteredCommand::from_definition(definition);
        if command.description.trim().is_empty() {
            return Err(CommandRegistrationError::EmptyDescription {
                command: command.name.clone(),
            });
        }
        if command.usage.trim().is_empty() {
            return Err(CommandRegistrationError::EmptyUsage {
                command: command.name.clone(),
            });
        }

        let mut command_lookups = HashSet::new();
        for lookup in command.lookups() {
            if !command_lookups.insert(lookup.clone()) {
                return Err(CommandRegistrationError::DuplicateLookup {
                    lookup: lookup.clone(),
                    command: command.name.clone(),
                    registered_command: command.name.clone(),
                });
            }
            if let Some(existing_index) = self.lookup.get(lookup) {
                return Err(CommandRegistrationError::DuplicateLookup {
                    lookup: lookup.clone(),
                    command: command.name.clone(),
                    registered_command: self.commands[*existing_index].name.clone(),
                });
            }
        }

        let command_index = self.commands.len();
        for lookup in command.lookups() {
            self.lookup.insert(lookup.clone(), command_index);
        }
        self.commands.push(command);
        Ok(())
    }

    /// Builds an immutable command catalog.
    pub fn build(self) -> CommandCatalog<TAction> {
        CommandCatalog {
            commands: self.commands,
            lookup: self.lookup,
        }
    }
}

impl<TAction> Default for CommandRegistrar<TAction> {
    fn default() -> Self {
        Self::new()
    }
}

/// Immutable command catalog used at runtime for lookup and introspection.
#[derive(Debug, Clone)]
pub struct CommandCatalog<TAction> {
    commands: Vec<RegisteredCommand<TAction>>,
    lookup: HashMap<CommandName, usize>,
}

impl<TAction> CommandCatalog<TAction> {
    /// Resolves a command by canonical name or alias.
    pub fn resolve(&self, name: &str) -> Option<&RegisteredCommand<TAction>> {
        let name = CommandName::new(name).ok()?;
        self.resolve_name(&name)
    }

    /// Resolves a command by validated name.
    pub fn resolve_name(&self, name: &CommandName) -> Option<&RegisteredCommand<TAction>> {
        self.lookup
            .get(name)
            .map(|command_index| &self.commands[*command_index])
    }

    /// Lists commands in registration order.
    pub fn list(&self) -> &[RegisteredCommand<TAction>] {
        &self.commands
    }
}

impl<TAction> RegisteredCommand<TAction> {
    fn lookups(&self) -> impl Iterator<Item = &CommandName> {
        std::iter::once(&self.name).chain(self.aliases.iter())
    }
}

fn validate_command_name(name: &str) -> Result<(), CommandNameError> {
    if name.trim() != name {
        return Err(CommandNameError {
            name: name.to_string(),
            reason: "names must not include surrounding whitespace",
        });
    }
    let Some(identifier) = name.strip_prefix('/') else {
        return Err(CommandNameError {
            name: name.to_string(),
            reason: "names must start with `/`",
        });
    };
    if identifier.is_empty() {
        return Err(CommandNameError {
            name: name.to_string(),
            reason: "names must include an identifier after `/`",
        });
    }

    let mut characters = identifier.chars();
    let Some(first) = characters.next() else {
        return Err(CommandNameError {
            name: name.to_string(),
            reason: "names must include an identifier after `/`",
        });
    };
    if !first.is_ascii_lowercase() {
        return Err(CommandNameError {
            name: name.to_string(),
            reason: "identifiers must start with an ASCII lowercase letter",
        });
    }
    for character in characters {
        if !(character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || character == '-'
            || character == '_')
        {
            return Err(CommandNameError {
                name: name.to_string(),
                reason: "identifiers must contain only ASCII lowercase letters, digits, `-`, or `_`",
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TestAction {
        Deploy,
        Status,
    }

    fn command(name: &str, action: TestAction) -> CommandDefinition<TestAction> {
        CommandDefinition::new(name, "description", format!("{name} usage"), action).unwrap()
    }

    #[test]
    fn resolves_registered_command_by_name_and_alias() {
        let mut registrar = CommandRegistrar::new();
        registrar
            .register(
                command("/deploy", TestAction::Deploy)
                    .with_alias("/release")
                    .unwrap(),
            )
            .unwrap();
        let catalog = registrar.build();

        assert_eq!(
            *catalog.resolve("/deploy").unwrap().action(),
            TestAction::Deploy
        );
        assert_eq!(
            *catalog.resolve("/release").unwrap().action(),
            TestAction::Deploy
        );
    }

    #[test]
    fn rejects_duplicate_name_or_alias_lookups() {
        let mut registrar = CommandRegistrar::new();
        registrar
            .register(command("/deploy", TestAction::Deploy))
            .unwrap();

        let error = registrar
            .register(
                command("/status", TestAction::Status)
                    .with_alias("/deploy")
                    .unwrap(),
            )
            .unwrap_err();

        assert_eq!(
            error,
            CommandRegistrationError::DuplicateLookup {
                lookup: CommandName::new("/deploy").unwrap(),
                command: CommandName::new("/status").unwrap(),
                registered_command: CommandName::new("/deploy").unwrap(),
            }
        );
    }

    #[test]
    fn rejects_invalid_names() {
        assert!(CommandName::new("deploy").is_err());
        assert!(CommandName::new("/Deploy").is_err());
        assert!(CommandName::new("/deploy now").is_err());
        assert!(CommandName::new("/").is_err());
    }

    #[test]
    fn preserves_registration_order_for_list() {
        let mut registrar = CommandRegistrar::new();
        registrar
            .register(command("/deploy", TestAction::Deploy))
            .unwrap();
        registrar
            .register(command("/status", TestAction::Status))
            .unwrap();
        let catalog = registrar.build();

        let names = catalog
            .list()
            .iter()
            .map(|command| command.name().as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["/deploy", "/status"]);
    }
}
