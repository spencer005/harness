//! Shell-style word splitting shared by inspect subcommands.

/// One shell-parsed word together with whether it was quoted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellWord {
    /// The decoded word value.
    pub value: String,
    /// Whether the word was produced inside quotes.
    pub quoted: bool,
}

/// Split a command line into shell-quoted words.
pub fn parse_shell_words(input: &str) -> Result<Vec<ShellWord>, String> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = Quote::None;
    let mut current_quoted = false;
    let mut chars = input.chars().peekable();
    let mut in_word = false;
    while let Some(character) = chars.next() {
        match (quote, character) {
            (Quote::None, '\'') => {
                quote = Quote::Single;
                current_quoted = true;
                in_word = true;
            }
            (Quote::None, '"') => {
                quote = Quote::Double;
                current_quoted = true;
                in_word = true;
            }
            (Quote::Single, '\'') => {
                quote = Quote::None;
            }
            (Quote::Double, '"') => {
                quote = Quote::None;
            }
            (Quote::None, '\\') => {
                let Some(escaped) = chars.next() else {
                    return Err("trailing backslash".to_string());
                };
                current.push(escaped);
                in_word = true;
            }
            (Quote::Double, '\\') => {
                let Some(next) = chars.next() else {
                    return Err("trailing backslash".to_string());
                };
                // Inside double quotes, backslash is only special before the
                // POSIX-defined set of characters. For anything else (e.g. \[)
                // both the backslash and the character are preserved literally,
                // so that regex tools like search receive \[ rather than a bare [.
                if matches!(next, '$' | '`' | '"' | '\\' | '\n') {
                    current.push(next);
                } else {
                    current.push('\\');
                    current.push(next);
                }
                in_word = true;
            }
            (Quote::None, character) if character.is_whitespace() => {
                if in_word {
                    words.push(ShellWord {
                        value: std::mem::take(&mut current),
                        quoted: current_quoted,
                    });
                    in_word = false;
                    current_quoted = false;
                }
            }
            (_, character) => {
                current.push(character);
                in_word = true;
            }
        }
    }

    match quote {
        Quote::None => {}
        Quote::Single => return Err("unterminated single quote".to_string()),
        Quote::Double => return Err("unterminated double quote".to_string()),
    }
    if in_word {
        words.push(ShellWord {
            value: current,
            quoted: current_quoted,
        });
    }
    Ok(words)
}

/// Return `true` when the argument is a shell operator that inspect does not
/// support inside a single command chain (for example `>`, `&&`, `;`).
pub fn shell_operator_arg(arg: &str) -> bool {
    matches!(arg, "|" | "||" | "&&" | ";" | ">" | ">>" | "<")
}

/// Parse a positive `usize` for an inspect option, returning a formatted
/// `inspect`-prefixed error on failure.
pub fn parse_positive_usize(command: &str, value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("`{command}` count must be a positive integer"))?;
    if parsed == 0 {
        return Err(format!("`{command}` count must be a positive integer"));
    }
    Ok(parsed)
}

/// Parse a positive `usize` for a range token, returning `Err(())` on failure
/// so callers can attach their own inspect-prefixed message.
pub fn parse_positive_usize_value(value: &str) -> Result<usize, ()> {
    let parsed = value.trim().parse::<usize>().map_err(|_| ())?;
    if parsed == 0 {
        return Err(());
    }
    Ok(parsed)
}