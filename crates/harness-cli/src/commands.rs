use std::{
    fmt,
    str::{FromStr, SplitWhitespace},
    sync::Arc,
};

pub type CommandResult<T = ()> = Result<T, CommandError>;

pub type CommandHandler<App, Output> =
    for<'a> fn(&mut App, CommandContext<'a, App, Output>) -> CommandResult<Output>;

pub struct CommandRegistry<App, Output> {
    commands: Box<[Command<App, Output>]>,
    names: Box<[NameEntry]>,
}

pub struct Command<App, Output> {
    name: Arc<str>,
    aliases: Box<[Arc<str>]>,
    summary: Box<str>,
    usage: Box<str>,
    hidden: bool,
    handler: CommandHandler<App, Output>,
}

struct NameEntry {
    name: Arc<str>,
    command_index: usize,
    visible: bool,
}

pub struct CommandSpec<App, Output> {
    name: Box<str>,
    aliases: Vec<Box<str>>,
    summary: Box<str>,
    usage: Box<str>,
    hidden: bool,
    handler: CommandHandler<App, Output>,
}

pub struct CommandRegistryBuilder<App, Output> {
    specs: Vec<CommandSpec<App, Output>>,
}

pub struct CommandContext<'a, App, Output> {
    /// Registry access makes implementing `/commands` straightforward.
    pub registry: &'a CommandRegistry<App, Output>,

    /// Complete original input, including `/`.
    pub full_line: &'a str,

    /// Name or alias actually entered by the user.
    pub invoked_as: &'a str,

    /// Everything following the command name.
    pub raw_args: &'a str,

    /// Zero-allocation whitespace argument parser.
    pub args: ArgCursor<'a>,
}

pub enum Dispatch<Output> {
    /// Input did not begin with `/`.
    NotCommand,

    /// A command ran successfully.
    Ran(Output),
}

#[derive(Clone)]
pub struct ArgCursor<'a> {
    inner: SplitWhitespace<'a>,
}

#[derive(Debug)]
pub enum BuildError {
    InvalidName(Box<str>),
    DuplicateName(Box<str>),
}

#[derive(Debug)]
pub enum CommandError {
    MissingCommand,
    UnknownCommand(Box<str>),
    MissingArgument {
        name: &'static str,
    },
    InvalidArgument {
        name: &'static str,
        value: Box<str>,
        reason: Box<str>,
    },
    UnexpectedArgument(Box<str>),
    Message(Box<str>),
}

// -----------------------------------------------------------------------------
// Command builder
// -----------------------------------------------------------------------------

impl<App, Output> CommandSpec<App, Output> {
    pub fn new(name: impl Into<Box<str>>, handler: CommandHandler<App, Output>) -> Self {
        Self {
            name: name.into(),
            aliases: Vec::new(),
            summary: "".into(),
            usage: "".into(),
            hidden: false,
            handler,
        }
    }

    pub fn alias(mut self, alias: impl Into<Box<str>>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    pub fn summary(mut self, summary: impl Into<Box<str>>) -> Self {
        self.summary = summary.into();
        self
    }

    pub fn usage(mut self, usage: impl Into<Box<str>>) -> Self {
        self.usage = usage.into();
        self
    }

    pub fn hidden(mut self) -> Self {
        self.hidden = true;
        self
    }
}

// -----------------------------------------------------------------------------
// Registry
// -----------------------------------------------------------------------------

impl<App, Output> CommandRegistry<App, Output> {
    pub fn builder() -> CommandRegistryBuilder<App, Output> {
        CommandRegistryBuilder { specs: Vec::new() }
    }

    pub fn dispatch(&self, app: &mut App, line: &str) -> CommandResult<Dispatch<Output>> {
        let Some(body) = line.strip_prefix('/') else {
            return Ok(Dispatch::NotCommand);
        };

        let body = body.trim_start();

        if body.is_empty() {
            return Err(CommandError::MissingCommand);
        }

        let split_at = body.find(char::is_whitespace).unwrap_or(body.len());

        let invoked_as = &body[..split_at];
        let raw_args = body[split_at..].trim_start();

        let command = self
            .find(invoked_as)
            .ok_or_else(|| CommandError::UnknownCommand(invoked_as.into()))?;

        let output = (command.handler)(
            app,
            CommandContext {
                registry: self,
                full_line: line,
                invoked_as,
                raw_args,
                args: ArgCursor::new(raw_args),
            },
        )?;

        Ok(Dispatch::Ran(output))
    }

    pub fn find(&self, name_or_alias: &str) -> Option<&Command<App, Output>> {
        let index = self
            .names
            .binary_search_by(|entry| entry.name.as_ref().cmp(name_or_alias))
            .ok()?;

        Some(&self.commands[self.names[index].command_index])
    }

    pub fn visible_commands(&self) -> impl Iterator<Item = &Command<App, Output>> {
        self.commands.iter().filter(|command| !command.hidden)
    }

    /// Returns command names and aliases beginning with `prefix`.
    ///
    /// Lookup is O(log n), followed by iteration over matching entries.
    pub fn complete_name<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        let start = self
            .names
            .partition_point(|entry| entry.name.as_ref() < prefix);

        self.names[start..]
            .iter()
            .take_while(move |entry| entry.name.starts_with(prefix))
            .filter(|entry| entry.visible)
            .map(|entry| entry.name.as_ref())
    }
}

impl<App, Output> CommandRegistryBuilder<App, Output> {
    pub fn command(mut self, command: CommandSpec<App, Output>) -> Self {
        self.specs.push(command);
        self
    }

    pub fn build(self) -> Result<CommandRegistry<App, Output>, BuildError> {
        let name_count = self.specs.iter().map(|spec| 1 + spec.aliases.len()).sum();

        let mut commands = Vec::with_capacity(self.specs.len());
        let mut names = Vec::with_capacity(name_count);

        for spec in self.specs {
            validate_name(&spec.name)?;

            for alias in &spec.aliases {
                validate_name(alias)?;
            }

            let command_index = commands.len();
            let name = Arc::<str>::from(spec.name);

            let aliases: Vec<Arc<str>> = spec.aliases.into_iter().map(Arc::<str>::from).collect();

            names.push(NameEntry {
                name: Arc::clone(&name),
                command_index,
                visible: !spec.hidden,
            });

            for alias in &aliases {
                names.push(NameEntry {
                    name: Arc::clone(alias),
                    command_index,
                    visible: !spec.hidden,
                });
            }

            commands.push(Command {
                name,
                aliases: aliases.into_boxed_slice(),
                summary: spec.summary,
                usage: spec.usage,
                hidden: spec.hidden,
                handler: spec.handler,
            });
        }

        names.sort_unstable_by(|left, right| left.name.as_ref().cmp(right.name.as_ref()));

        for duplicate in names.windows(2) {
            if duplicate[0].name == duplicate[1].name {
                return Err(BuildError::DuplicateName(duplicate[0].name.as_ref().into()));
            }
        }

        Ok(CommandRegistry {
            commands: commands.into_boxed_slice(),
            names: names.into_boxed_slice(),
        })
    }
}

// -----------------------------------------------------------------------------
// Argument parsing
// -----------------------------------------------------------------------------

impl<'a> ArgCursor<'a> {
    pub fn new(raw: &'a str) -> Self {
        Self {
            inner: raw.split_whitespace(),
        }
    }

    pub fn required(&mut self, name: &'static str) -> CommandResult<&'a str> {
        self.next().ok_or(CommandError::MissingArgument { name })
    }

    pub fn parse<T>(&mut self, name: &'static str) -> CommandResult<T>
    where
        T: FromStr,
        T::Err: fmt::Display,
    {
        let value = self.required(name)?;

        value
            .parse()
            .map_err(|error: T::Err| CommandError::InvalidArgument {
                name,
                value: value.into(),
                reason: error.to_string().into_boxed_str(),
            })
    }

    pub fn parse_or<T>(&mut self, name: &'static str, default: T) -> CommandResult<T>
    where
        T: FromStr,
        T::Err: fmt::Display,
    {
        let Some(value) = self.next() else {
            return Ok(default);
        };

        value
            .parse()
            .map_err(|error: T::Err| CommandError::InvalidArgument {
                name,
                value: value.into(),
                reason: error.to_string().into_boxed_str(),
            })
    }

    /// Reject any unconsumed arguments.
    pub fn finish(mut self) -> CommandResult {
        match self.next() {
            Some(extra) => Err(CommandError::UnexpectedArgument(extra.into())),
            None => Ok(()),
        }
    }
}

impl<'a> Iterator for ArgCursor<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

// -----------------------------------------------------------------------------
// Metadata
// -----------------------------------------------------------------------------

impl<App, Output> Command<App, Output> {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn aliases(&self) -> impl Iterator<Item = &str> {
        self.aliases.iter().map(|alias| alias.as_ref())
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn usage(&self) -> &str {
        &self.usage
    }
}

// -----------------------------------------------------------------------------
// Validation and errors
// -----------------------------------------------------------------------------

fn validate_name(name: &str) -> Result<(), BuildError> {
    let valid = !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        });

    if valid {
        Ok(())
    } else {
        Err(BuildError::InvalidName(name.into()))
    }
}

impl CommandError {
    pub fn message(message: impl Into<Box<str>>) -> Self {
        Self::Message(message.into())
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(name) => {
                write!(f, "invalid command name: {name}")
            }
            Self::DuplicateName(name) => {
                write!(f, "duplicate command or alias: {name}")
            }
        }
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCommand => {
                write!(f, "missing command after '/'")
            }
            Self::UnknownCommand(name) => {
                write!(f, "unknown command: /{name}")
            }
            Self::MissingArgument { name } => {
                write!(f, "missing argument: {name}")
            }
            Self::InvalidArgument {
                name,
                value,
                reason,
            } => {
                write!(f, "invalid {name} '{value}': {reason}")
            }
            Self::UnexpectedArgument(value) => {
                write!(f, "unexpected argument: {value}")
            }
            Self::Message(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for BuildError {}
impl std::error::Error for CommandError {}
