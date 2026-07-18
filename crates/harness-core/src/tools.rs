use serde::{Deserialize, Serialize};
use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};
use thiserror::Error;

/// Responses API tool type for freeform/custom tools.
pub const FREEFORM_TOOL_TYPE: &str = "custom";
/// Freeform tool format kind used by Codex for grammar-constrained tools.
pub const FREEFORM_TOOL_FORMAT_TYPE: &str = "grammar";
/// Grammar syntax used by Codex freeform `apply_patch`.
pub const FREEFORM_TOOL_FORMAT_SYNTAX_LARK: &str = "lark";
/// Responses output-item type emitted when the model calls a freeform tool.
pub const FREEFORM_TOOL_CALL_TYPE: &str = "custom_tool_call";
/// Responses input-item type used to return a freeform tool result.
pub const FREEFORM_TOOL_CALL_OUTPUT_TYPE: &str = "custom_tool_call_output";
/// Responses stream event carrying a raw input fragment for a freeform tool.
pub const RESPONSE_CUSTOM_TOOL_CALL_INPUT_DELTA_TYPE: &str =
    "response.custom_tool_call_input.delta";
/// Responses stream event carrying a JSON/function argument fragment.
pub const RESPONSE_FUNCTION_CALL_ARGUMENTS_DELTA_TYPE: &str =
    "response.function_call_arguments.delta";
/// Responses stream event carrying an output item when it is first added.
pub const RESPONSE_OUTPUT_ITEM_ADDED_TYPE: &str = "response.output_item.added";
/// Responses stream event carrying an output item after it is complete.
pub const RESPONSE_OUTPUT_ITEM_DONE_TYPE: &str = "response.output_item.done";
/// Native freeform apply-patch tool name.
pub const APPLY_PATCH_TOOL_NAME: &str = "apply_patch";
/// Root-mode file-edit worker tool name.
pub const EDIT_FILE_TOOL_NAME: &str = "edit_file";
/// Root-mode implementation locator tool name.
pub const LOCATE_TOOL_NAME: &str = "locate";
/// Unified inspection tool name.
pub const INSPECT_TOOL_NAME: &str = "inspect";
/// Root-mode staged patch apply tool name.
pub const STAGED_PATCH_APPLY_TOOL_NAME: &str = "staged_patch_apply";
/// Root-mode staged patch discard tool name.
pub const STAGED_PATCH_DISCARD_TOOL_NAME: &str = "staged_patch_discard";
/// PTY terminal creation tool name.
pub const TERMINAL_OPEN_TOOL_NAME: &str = "terminal_open";
/// PTY terminal input tool name.
pub const TERMINAL_WRITE_TOOL_NAME: &str = "terminal_write";
/// PTY terminal output polling tool name.
pub const TERMINAL_READ_TOOL_NAME: &str = "terminal_read";
/// Persist-mode task completion marker tool name.
pub const MARK_TASK_COMPLETE_TOOL_NAME: &str = "mark_task_complete";
/// Responses API tool type for JSON/function tools.
pub const FUNCTION_TOOL_TYPE: &str = "function";
/// Responses output-item type emitted when the model calls a function tool.
pub const FUNCTION_TOOL_CALL_TYPE: &str = "function_call";

/// Lark grammar used by Codex's native freeform `apply_patch` tool.
pub const APPLY_PATCH_LARK_GRAMMAR: &str = r#"start: begin_patch hunk+ end_patch
begin_patch: "*** Begin Patch" LF
end_patch: "*** End Patch" LF?

hunk: add_hunk | delete_hunk | update_hunk
add_hunk: "*** Add File: " filename LF add_line+
delete_hunk: "*** Delete File: " filename LF
update_hunk: "*** Update File: " filename LF change_move? change?

filename: /(.+)/
add_line: "+" /(.*)/ LF -> line

change_move: "*** Move to: " filename LF
change: (change_context | change_line)+ eof_line?
change_context: ("@@" | "@@ " /(.+)/) LF
change_line: ("+" | "-" | " ") /(.*)/ LF
eof_line: "*** End of File" LF

%import common.LF
"#;

/// Lark grammar for native line-anchor file edits.
pub const EDIT_FILE_LARK_GRAMMAR: &str = r#"start: operation+
operation: add | remove | move | edit
add: "§ Add " path LF body
remove: "§ Remove " path LF?
move: "§ Move " path LF "§ To " path LF?
edit: "§ Edit " path LF body

body: /[\s\S]+/
path: /[^\n]+/

%import common.LF
"#;

/// Lark grammar for root-owned implementation location queries.
pub const LOCATE_LARK_GRAMMAR: &str = r#"start: query
query: /[\s\S]+/
"#;
/// Lark grammar for the unified inspection tool.
pub const INSPECT_LARK_GRAMMAR: &str = r#"start: input
input: /[\s\S]+/
"#;

/// Lark grammar for applying or discarding an in-memory staged patch.
pub const STAGED_PATCH_LARK_GRAMMAR: &str = r#"start: "patch:" value LF?

value: /[^\n]+/

%import common.LF
"#;

/// Lark grammar for opening a command-isolated PTY terminal session.
pub const TERMINAL_OPEN_LARK_GRAMMAR: &str = r#"start: open_field* command_tail
open_field: option_line
option_line: option_key ":" value LF?
option_key: "workdir" | "rows" | "cols"
command_tail: command_inline
            | command_block
command_inline: "command:" inline_value LF?
command_block: "command:" LF body

value: /[^\n]*/
inline_value: /[^\n]+/
body: /[\s\S]+/

%import common.LF
"#;

/// Lark grammar for writing interactive input to a running terminal command.
pub const TERMINAL_WRITE_LARK_GRAMMAR: &str = r#"start: terminal_line input_tail
     | input_inline terminal_line
input_tail: input_inline
          | input_block
terminal_line: "terminal:" value LF?
input_inline: "input:" inline_value LF?
input_block: "input:" LF body
           | "input:" LF?

value: /[^\n]*/
inline_value: /[^\n]+/
body: /[\s\S]+/

%import common.LF
"#;

/// Lark grammar for reading recent PTY terminal output.
pub const TERMINAL_READ_LARK_GRAMMAR: &str = r#"start: poll_line? terminal_line poll_line?
poll_line: "poll_after:" value LF?
terminal_line: "terminal:" value LF?

value: /[^\n]+/

%import common.LF
"#;

/// Lark grammar for the persist-mode task completion marker.
pub const MARK_TASK_COMPLETE_LARK_GRAMMAR: &str = r#"start: "complete" LF?

%import common.LF
"#;

/// Native Responses freeform/custom tool definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreeformTool {
    /// Tool name shown to the model.
    pub name: String,
    /// Tool description shown to the model.
    pub description: String,
    /// Grammar/format contract for the raw freeform input.
    pub format: FreeformToolFormat,
}

/// Grammar format metadata for a freeform/custom tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreeformToolFormat {
    /// Format type, currently `grammar`.
    #[serde(rename = "type")]
    pub format_type: String,
    /// Grammar syntax, currently `lark`.
    pub syntax: String,
    /// Grammar definition consumed by the Responses API.
    pub definition: String,
}

impl FreeformTool {
    /// Build Codex's current native freeform `apply_patch` tool spec.
    ///
    /// This intentionally serializes as a Responses API `type: "custom"` tool
    /// with a Lark grammar. The model sends raw patch text in
    /// `custom_tool_call.input`; it must not be wrapped in JSON arguments.
    pub fn apply_patch() -> Self {
        Self {
            name: APPLY_PATCH_TOOL_NAME.to_string(),
            description:
                "Use the `apply_patch` tool to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON."
                    .to_string(),
            format: FreeformToolFormat {
                format_type: FREEFORM_TOOL_FORMAT_TYPE.to_string(),
                syntax: FREEFORM_TOOL_FORMAT_SYNTAX_LARK.to_string(),
                definition: APPLY_PATCH_LARK_GRAMMAR.to_string(),
            },
        }
    }

    /// Build the native line-anchor file edit tool spec.
    pub fn edit_file() -> Self {
        Self {
            name: EDIT_FILE_TOOL_NAME.to_string(),
            description: "Edit files using line anchors from `inspect` read output. Use raw lines, not JSON. Uses `§` section headers: `§ Edit <path>`, `§ Add <path>`, `§ Remove <path>`, and `§ Move <old_path>` followed by `§ To <new_path>`. Inside `§ Edit`, segment headers are `§ Replace <start_anchor> <end_anchor>`, `§ Delete <start_anchor> <end_anchor>`, `§ Before <anchor>`, `§ After <anchor>`, and `§ Append <last_line_anchor>`. A segment body continues until the next `§` header or end of input. Every literal `§` in a body must be escaped as `\\§`; the escape is removed from written content. Anchors use a positive line number followed by one vocabulary word, e.g. `24 bucket`. Replace/Delete ranges are inclusive. `***` patch delimiters are invalid. Anchors refer to the file state before this edit; re-read after mutating segments in the same call."
                .to_string(),
            format: FreeformToolFormat {
                format_type: FREEFORM_TOOL_FORMAT_TYPE.to_string(),
                syntax: FREEFORM_TOOL_FORMAT_SYNTAX_LARK.to_string(),
                definition: EDIT_FILE_LARK_GRAMMAR.to_string(),
            },
        }
    }

    /// Build the root-mode implementation locator tool spec.
    pub fn locate() -> Self {
        Self {
            name: LOCATE_TOOL_NAME.to_string(),
            description: "Get context. If know nothing, ask directly: `need context for [your task]`. If enough known, ask specific: `how thing1 implemented`, `what is /path/file`, `all data structures for feature`, or `how edit_file implemented`. Locator tool no omniscient; pass semantically required info for good result. Get context only. Do not ask locator to make decisions. Bad: `how should this be implemented`. Better: `give all info needed to understand x implementation`."
                .to_string(),
            format: FreeformToolFormat {
                format_type: FREEFORM_TOOL_FORMAT_TYPE.to_string(),
                syntax: FREEFORM_TOOL_FORMAT_SYNTAX_LARK.to_string(),
                definition: LOCATE_LARK_GRAMMAR.to_string(),
            },
        }
    }

    /// Build the unified inspection tool spec.
    pub fn inspect() -> Self {
        Self {
            name: INSPECT_TOOL_NAME.to_string(),
            description: "Batch compact inspection jobs. Each command-start line runs as an independent job.\nread <path> [range ...] — Print file lines. Ranges are `start+count` or `start-end`; pass several to batch. Quoted paths are supported. Each output line is prefixed with an anchor word; use `<line> <word>` as `edit_file` anchors.\nlist [path] [--depth <n>] [--exact] — List directory entries. Direct children are shown by default; directories end in `/`, symlinks use `->`, text files show line counts, and other files show rounded whole-unit sizes. `--exact` also shows exact byte sizes for text files and uses exact sizes for other files. Output is limited to 500 entries.\nstat <path> [path ...] [--exact] [--metadata] — Show stat-like size, modification time, and permissions without following symlinks. `--exact` prints exact byte sizes and subsecond timestamps. `--metadata` includes uncommon ownership, inode, device, link, access/change time, and block fields.\nbytes <path> <offset>+<length> — Read a bounded byte range as contiguous lowercase hex without separators. Recognize it; don't decode it. Your immediate read is usually right. Decode byte by byte only to resolve ambiguity or verify an exact detail. Reads are limited to 16384 bytes; offsets and requested lengths use exact byte counts, while total size uses a rounded whole-unit representation.\nbyte-search <path> <hex> — Find an exact contiguous hex sequence in a file. Returns exact byte offsets, including overlapping matches; output is limited to 100 offsets.\nstrings <path> [literal] — Index printable UTF-8 runs of at least four characters and return exact byte offsets. An optional literal filters whole runs. Output is limited to 100 strings and 160 preview characters per string.\nelf <path> [summary|sections|segments|symbols [literal]|relocations [literal]|dynamic [literal]|address <virtual>|offset <file>] — Inspect ELF structure without disassembling. The default summary reports class, architecture, endianness, kind, entry mapping, and interpreter. Other queries report exact file/virtual ranges, symbols, relocations, dynamic tags/imports, or translate between virtual addresses and file offsets. Symbol, relocation, and dynamic literals filter output.\nsearch <pattern> [path] — Regex search; returns matches with file names grouped once and line numbers. Patterns with regex metacharacters are treated as regex; plain identifiers are literal. Example: `search fn .*(needle|pin) src` or `search TODO src --exclude *.generated.rs`. Options: `-F` literal, `-i` ignore-case, `-g/--glob <glob>` include filter, `--exclude <glob>` exclude filter (repeatable), `--files` list paths.\nwhich <query> — Fuzzy-search executable commands in PATH and print command names with resolved paths.\ncheck [pkg ...] [--lib] [--all-targets] — Rust compiler diagnostics grouped by file under the `E0 err lineposition` header. Both flags work independently and together with package names.\ntest [cargo selectors] [filter ...] [-- libtest options] — Run `cargo test` with compact output. No filter runs the entire selected suite. Multiple positional filters run independently, which stock `cargo test` cannot do in one invocation. Cargo selectors include `-p/--package`, `--workspace`, `--lib`, `--bin`, `--test`, `--doc`, and `--all-targets`. Example: `test -p harness-core module::first module::second -- --exact`.\nps [name] — List processes; pass a name to filter instead of dumping all.\npwd — Print working directory."
                .to_string(),
            format: FreeformToolFormat {
                format_type: FREEFORM_TOOL_FORMAT_TYPE.to_string(),
                syntax: FREEFORM_TOOL_FORMAT_SYNTAX_LARK.to_string(),
                definition: INSPECT_LARK_GRAMMAR.to_string(),
            },
        }
    }

    /// Build the root-mode staged patch apply tool spec.
    pub fn staged_patch_apply() -> Self {
        Self {
            name: STAGED_PATCH_APPLY_TOOL_NAME.to_string(),
            description: "Apply an in-memory staged patch. Use raw lines, not JSON. Format: `patch: <staged_patch_id>`. This mutates the workspace only after the patch is revalidated against the current filesystem."
                .to_string(),
            format: FreeformToolFormat {
                format_type: FREEFORM_TOOL_FORMAT_TYPE.to_string(),
                syntax: FREEFORM_TOOL_FORMAT_SYNTAX_LARK.to_string(),
                definition: STAGED_PATCH_LARK_GRAMMAR.to_string(),
            },
        }
    }

    /// Build the root-mode staged patch discard tool spec.
    pub fn staged_patch_discard() -> Self {
        Self {
            name: STAGED_PATCH_DISCARD_TOOL_NAME.to_string(),
            description: "Discard an in-memory staged patch without mutating the workspace. Use raw lines, not JSON. Format: `patch: <staged_patch_id>`."
                .to_string(),
            format: FreeformToolFormat {
                format_type: FREEFORM_TOOL_FORMAT_TYPE.to_string(),
                syntax: FREEFORM_TOOL_FORMAT_SYNTAX_LARK.to_string(),
                definition: STAGED_PATCH_LARK_GRAMMAR.to_string(),
            },
        }
    }

    /// Build the native custom `terminal_open` tool spec.
    ///
    /// The model sends raw `key: value` text in `custom_tool_call.input`; it
    /// must not be wrapped in JSON arguments.
    pub fn terminal_open() -> Self {
        Self {
            name: TERMINAL_OPEN_TOOL_NAME.to_string(),
            description: "Starts one command in a fresh strict Bash process attached to a PTY. Use raw lines, not JSON. `command:` is required; optional fields are `workdir:`, `rows`, and `cols`. Bash enables `errexit`, `errtrace`, `nounset`, `pipefail`, `inherit_errexit`, and `failglob` before parsing the command. Shell variables, functions, aliases, option changes, and working-directory changes cannot survive into another tool call. If the command remains active, the returned terminal ID accepts interactive stdin through `terminal_write`. Start every subsequent command with a new `terminal_open`. Terminal output is returned with ANSI/color/control sequences filtered. Large terminal outputs are automatically summarized for the model; use `inspect` read for explicit ranged file reads."
                .to_string(),
            format: FreeformToolFormat {
                format_type: FREEFORM_TOOL_FORMAT_TYPE.to_string(),
                syntax: FREEFORM_TOOL_FORMAT_SYNTAX_LARK.to_string(),
                definition: TERMINAL_OPEN_LARK_GRAMMAR.to_string(),
            },
        }
    }

    /// Build the native custom `terminal_write` tool spec.
    ///
    /// The model sends raw `key: value` text in `custom_tool_call.input`; it
    /// must not be wrapped in JSON arguments.
    pub fn terminal_write() -> Self {
        Self {
            name: TERMINAL_WRITE_TOOL_NAME.to_string(),
            description: "Writes interactive stdin to the one command running in an existing PTY terminal. Use raw lines, not JSON. `terminal:` and `input:` are required. This tool never starts shell commands; start every command with `terminal_open`. Printable input that does not end with a newline is submitted automatically. If the process has already exited, its final output and exit status are returned without sending the input. Output is returned with ANSI/color/control sequences filtered. Large terminal outputs are automatically summarized for the model; use `inspect` read for explicit ranged file reads."
                .to_string(),
            format: FreeformToolFormat {
                format_type: FREEFORM_TOOL_FORMAT_TYPE.to_string(),
                syntax: FREEFORM_TOOL_FORMAT_SYNTAX_LARK.to_string(),
                definition: TERMINAL_WRITE_LARK_GRAMMAR.to_string(),
            },
        }
    }

    /// Build the native custom `terminal_read` tool spec.
    ///
    /// The model sends raw `key: value` text in `custom_tool_call.input`; it
    /// must not be wrapped in JSON arguments.
    pub fn terminal_read() -> Self {
        Self {
            name: TERMINAL_READ_TOOL_NAME.to_string(),
            description: "Reads recent output from an existing PTY terminal without sending input. Use raw lines, not JSON. `terminal:` is required. Optional `poll_after: <duration>` such as `30s` or `250ms` waits for that full interval before returning accumulated output; intermediate output does not wake the call, but command or terminal exit does. Without `poll_after`, the call returns after output settles or a short internal wait. Output is returned with ANSI/color/control sequences filtered. Large terminal outputs are automatically summarized for the model; use `inspect` read for explicit ranged file reads."
                .to_string(),
            format: FreeformToolFormat {
                format_type: FREEFORM_TOOL_FORMAT_TYPE.to_string(),
                syntax: FREEFORM_TOOL_FORMAT_SYNTAX_LARK.to_string(),
                definition: TERMINAL_READ_LARK_GRAMMAR.to_string(),
            },
        }
    }

    /// Build the persist-mode task completion custom tool spec.
    ///
    /// The model sends exactly `complete` in `custom_tool_call.input`; the
    /// runtime consumes the call as a control signal and does not submit a tool
    /// output.
    pub fn mark_task_complete() -> Self {
        Self {
            name: MARK_TASK_COMPLETE_TOOL_NAME.to_string(),
            description: "Mark the current persisted task complete after verifying completion criteria. This control tool has no output and stops persist-mode continuation. Use exactly `complete`."
                .to_string(),
            format: FreeformToolFormat {
                format_type: FREEFORM_TOOL_FORMAT_TYPE.to_string(),
                syntax: FREEFORM_TOOL_FORMAT_SYNTAX_LARK.to_string(),
                definition: MARK_TASK_COMPLETE_LARK_GRAMMAR.to_string(),
            },
        }
    }
}

/// Responses API JSON/function tool definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionTool {
    /// Tool name shown to the model.
    pub name: String,
    /// Tool description shown to the model.
    pub description: String,
    /// Structured-output strictness flag.
    pub strict: bool,
    /// JSON schema for function arguments.
    pub parameters: Value,
}

impl FunctionTool {
    /// Create a JSON/function wrapper for a native freeform tool.
    pub fn from_freeform_tool(tool: &FreeformTool) -> Self {
        Self {
            name: tool.name.clone(),
            description: format!(
                "Execute the native `{}` tool through a JSON/function wrapper. \
                 Native tool instructions: {} \
                 Call this function with exactly one argument named `input`, whose value is a single JSON string containing the complete raw tool input. \
                 The raw input placed inside that JSON string must obey the tool's native grammar below; the wrapper itself requires valid JSON. \
                 For multi-line or block-style raw inputs, include literal newline characters (`\\n`) in the JSON string, including any `input:` header line and a trailing newline when the native tool requires one.",
                tool.name, tool.description
            ),
            strict: true,
            parameters: function_wrapped_freeform_parameters(tool),
        }
    }
}

#[derive(Debug, Deserialize)]
struct FunctionWrappedFreeformArguments {
    input: String,
}

fn function_wrapped_freeform_parameters(tool: &FreeformTool) -> Value {
    sonic_rs::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "input": {
                "type": "string",
                "description": format!(
                    "The complete raw input for the `{}` tool, passed as a single JSON string. \
                     Native tool instructions: {} \
                     Include literal newlines for block-style inputs (for example, keep the `input:` header on its own line followed by the body). \
                     The value must be the exact raw tool input and must obey this native Lark grammar:\n{}",
                    tool.name,
                    tool.description,
                    tool.format.definition
                )
            }
        },
        "required": ["input"]
    })
}

/// Parse the raw freeform input from a JSON/function wrapper call.
pub fn parse_function_wrapped_freeform_input(arguments: &str) -> Result<String, sonic_rs::Error> {
    sonic_rs::from_str::<FunctionWrappedFreeformArguments>(arguments)
        .map(|arguments| arguments.input)
}

/// Native tool advertised to the Responses API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NativeTool {
    /// Responses custom/freeform tool.
    Freeform(FreeformTool),
    /// Responses JSON/function tool.
    Function(FunctionTool),
}

impl NativeTool {
    /// Create the freeform `apply_patch` native tool.
    pub fn apply_patch() -> Self {
        Self::Freeform(FreeformTool::apply_patch())
    }

    /// Create the native line-anchor file edit tool.
    pub fn edit_file() -> Self {
        Self::Freeform(FreeformTool::edit_file())
    }

    /// Create the root-mode implementation locator native tool.
    pub fn locate() -> Self {
        Self::Freeform(FreeformTool::locate())
    }

    /// Create the unified inspection native tool.
    pub fn inspect() -> Self {
        Self::Freeform(FreeformTool::inspect())
    }

    /// Create the root-mode staged patch apply native tool.
    pub fn staged_patch_apply() -> Self {
        Self::Freeform(FreeformTool::staged_patch_apply())
    }

    /// Create the root-mode staged patch discard native tool.
    pub fn staged_patch_discard() -> Self {
        Self::Freeform(FreeformTool::staged_patch_discard())
    }

    /// Return the advertised native tool name.
    pub fn name(&self) -> &str {
        match self {
            Self::Freeform(tool) => &tool.name,
            Self::Function(tool) => &tool.name,
        }
    }

    /// Create the freeform PTY terminal open native tool.
    pub fn terminal_open() -> Self {
        Self::Freeform(FreeformTool::terminal_open())
    }

    /// Create the freeform PTY terminal write native tool.
    pub fn terminal_write() -> Self {
        Self::Freeform(FreeformTool::terminal_write())
    }

    /// Create the freeform PTY terminal read native tool.
    pub fn terminal_read() -> Self {
        Self::Freeform(FreeformTool::terminal_read())
    }

    /// Create the freeform persist-mode task completion marker native tool.
    pub fn mark_task_complete() -> Self {
        Self::Freeform(FreeformTool::mark_task_complete())
    }

    /// Return the default Codex native tool set.
    pub fn codex_native_tools() -> Vec<Self> {
        NativeToolRegistry::codex().tools()
    }

    /// Return the default Ollama Cloud native tool set.
    pub fn ollama_cloud_native_tools() -> Vec<Self> {
        NativeToolRegistry::ollama_cloud().tools()
    }
}

/// In-process handler used to execute an advertised native tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeToolHandler {
    /// Execute the tool with the native apply-patch engine.
    ApplyPatch,
    /// Execute a native line-anchor file edit.
    EditFile,
    /// Execute a root-owned implementation locator worker.
    Locate,
    /// Execute unified inspection jobs.
    Inspect,
    /// Apply a root staged patch.
    StagedPatchApply,
    /// Discard a root staged patch.
    StagedPatchDiscard,
    /// Execute the tool with the PTY-backed terminal manager.
    Terminal,
    /// Mark a persist-mode root task complete.
    MarkTaskComplete,
}

/// Native tool execution output split by consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeToolExecutionOutput {
    /// Output submitted back to the model.
    pub model_output: String,
    /// Output rendered in the transcript.
    pub display_output: String,
    /// Structured transcript display data.
    pub display: Option<crate::sessions::ToolOutputDisplayRecord>,
}

impl NativeToolExecutionOutput {
    /// Builds output where model and transcript consumers receive identical text.
    pub fn same(output: impl Into<String>) -> Self {
        let output = output.into();
        Self {
            model_output: output.clone(),
            display_output: output,
            display: None,
        }
    }

    /// Builds output with separate model and transcript text.
    pub fn split(model_output: String, display_output: String) -> Self {
        Self {
            model_output,
            display_output,
            display: None,
        }
    }
    /// Attaches structured transcript display data.
    pub fn with_structured_display(
        mut self,
        display: Option<crate::sessions::ToolOutputDisplayRecord>,
    ) -> Self {
        self.display = display;
        self
    }
}

/// Native tool definition paired with its in-process executor.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeToolEntry {
    /// Tool definition advertised to the model.
    pub tool: NativeTool,
    /// In-process handler that executes the tool.
    pub handler: NativeToolHandler,
}

impl NativeToolEntry {
    /// Pair an advertised native tool with its executor.
    pub fn new(tool: NativeTool, handler: NativeToolHandler) -> Self {
        Self { tool, handler }
    }

    /// Return the advertised tool name.
    pub fn name(&self) -> &str {
        self.tool.name()
    }
}

/// In-process native tool catalog and dispatch table.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NativeToolRegistry {
    entries: Vec<NativeToolEntry>,
}

impl NativeToolRegistry {
    /// Create a registry from explicit entries.
    pub fn new(entries: Vec<NativeToolEntry>) -> Self {
        Self { entries }
    }

    /// Create the default Codex tool registry.
    pub fn codex() -> Self {
        Self::new(vec![
            NativeToolEntry::new(NativeTool::locate(), NativeToolHandler::Locate),
            NativeToolEntry::new(NativeTool::inspect(), NativeToolHandler::Inspect),
            NativeToolEntry::new(NativeTool::edit_file(), NativeToolHandler::EditFile),
            NativeToolEntry::new(NativeTool::terminal_open(), NativeToolHandler::Terminal),
            NativeToolEntry::new(NativeTool::terminal_write(), NativeToolHandler::Terminal),
            NativeToolEntry::new(NativeTool::terminal_read(), NativeToolHandler::Terminal),
            NativeToolEntry::new(
                NativeTool::mark_task_complete(),
                NativeToolHandler::MarkTaskComplete,
            ),
        ])
    }

    /// Create the default Ollama Cloud tool registry.
    pub fn ollama_cloud() -> Self {
        Self::codex().with_function_wrapped_freeform_tools()
    }

    /// Create a registry that only exposes `apply_patch`.
    pub fn apply_patch_only() -> Self {
        Self::new(vec![NativeToolEntry::new(
            NativeTool::apply_patch(),
            NativeToolHandler::ApplyPatch,
        )])
    }

    /// Create an Ollama Cloud registry that only exposes `apply_patch`.
    pub fn apply_patch_only_for_ollama_cloud() -> Self {
        Self::apply_patch_only().with_function_wrapped_freeform_tools()
    }

    /// Return a registry with all PTY terminal tools removed.
    pub fn without_terminal_tools(&self) -> Self {
        Self::new(
            self.entries
                .iter()
                .filter(|entry| !is_terminal_tool_name(entry.name()))
                .cloned()
                .collect(),
        )
    }

    /// Return whether any PTY terminal tool is advertised.
    pub fn has_terminal_tools(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| is_terminal_tool_name(entry.name()))
    }

    /// Return all registry entries.
    pub fn entries(&self) -> &[NativeToolEntry] {
        &self.entries
    }

    /// Return all advertised tool definitions.
    pub fn tools(&self) -> Vec<NativeTool> {
        self.entries
            .iter()
            .map(|entry| entry.tool.clone())
            .collect()
    }

    /// Return a registry whose freeform tools are advertised as JSON/function tools.
    pub fn with_function_wrapped_freeform_tools(&self) -> Self {
        Self::new(
            self.entries
                .iter()
                .map(|entry| {
                    let tool = match &entry.tool {
                        NativeTool::Freeform(tool) => {
                            NativeTool::Function(FunctionTool::from_freeform_tool(tool))
                        }
                        NativeTool::Function(tool) => NativeTool::Function(tool.clone()),
                    };
                    NativeToolEntry::new(tool, entry.handler)
                })
                .collect(),
        )
    }

    /// Return all advertised tool names.
    pub fn tool_names(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.name().to_string())
            .collect()
    }

    /// Return the handler for an advertised tool name.
    pub fn handler_for(&self, name: &str) -> Option<NativeToolHandler> {
        self.entries
            .iter()
            .find(|entry| entry.name() == name)
            .map(|entry| entry.handler)
    }

    /// Return whether the named native tool is advertised as a JSON/function tool.
    pub fn advertises_function_tool(&self, name: &str) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.name() == name && matches!(entry.tool, NativeTool::Function(_)))
    }
}

/// Return whether the named tool is a PTY-backed terminal tool.
fn is_terminal_tool_name(name: &str) -> bool {
    matches!(
        name,
        TERMINAL_OPEN_TOOL_NAME | TERMINAL_WRITE_TOOL_NAME | TERMINAL_READ_TOOL_NAME
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ResponsesFreeformTool<'a> {
    #[serde(rename = "type")]
    tool_type: &'static str,
    name: &'a str,
    description: &'a str,
    format: &'a FreeformToolFormat,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct ResponsesFunctionTool<'a> {
    #[serde(rename = "type")]
    tool_type: &'static str,
    name: &'a str,
    description: &'a str,
    strict: bool,
    parameters: &'a Value,
}

/// Serialize native tools for `response.create.tools`.
pub fn create_tools_json_for_responses_api(
    tools: &[NativeTool],
) -> Result<Vec<Value>, sonic_rs::Error> {
    tools
        .iter()
        .map(|tool| match tool {
            NativeTool::Freeform(tool) => sonic_rs::to_value(&ResponsesFreeformTool {
                tool_type: FREEFORM_TOOL_TYPE,
                name: &tool.name,
                description: &tool.description,
                format: &tool.format,
            }),
            NativeTool::Function(tool) => sonic_rs::to_value(&ResponsesFunctionTool {
                tool_type: FUNCTION_TOOL_TYPE,
                name: &tool.name,
                description: &tool.description,
                strict: tool.strict,
                parameters: &tool.parameters,
            }),
        })
        .collect()
}

/// Completed freeform/custom tool call parsed from a Responses frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreeformToolCall {
    /// Responses API call id used when sending `custom_tool_call_output`.
    pub call_id: String,
    /// Freeform tool name, for example `apply_patch`.
    pub name: String,
    /// Raw model-produced input. For `apply_patch`, this is the patch text.
    pub input: String,
}

/// Streaming input delta for a freeform/custom tool call.
///
/// Codex's freeform stream uses `response.custom_tool_call_input.delta`. That
/// is a different event from legacy JSON/function tools, which stream
/// `response.function_call_arguments.delta`. This type only represents the
/// custom/freeform delta path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreeformToolInputDelta {
    /// Output item id when present; Codex falls back to `call_id` if omitted.
    pub item_id: String,
    /// Responses API call id, when present on the delta frame.
    pub call_id: Option<String>,
    /// Raw input text fragment.
    pub delta: String,
}

/// Final JSON/function tool call emitted by the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionToolCall {
    /// Responses API call id used when sending a function result.
    pub call_id: String,
    /// Function tool name.
    pub name: String,
    /// Raw JSON arguments string.
    pub arguments: String,
}

/// Streaming JSON/function argument delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionToolInputDelta {
    /// Output item id when present; Codex falls back to `call_id` if omitted.
    pub item_id: String,
    /// Responses API call id, when present on the delta frame.
    pub call_id: Option<String>,
    /// Raw JSON argument fragment.
    pub delta: String,
}

/// Error raised while parsing native freeform tool stream items.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum NativeToolError {
    /// Expected a JSON object but received another JSON value.
    #[error("response item must be a JSON object")]
    ItemNotObject,
    /// Required JSON field is absent.
    #[error("response item is missing JSON field `{field}`")]
    MissingJsonField {
        /// Missing field name.
        field: &'static str,
    },
    /// Required string field is absent or not a string.
    #[error("response item is missing string field `{field}`")]
    MissingStringField {
        /// Missing or non-string field name.
        field: &'static str,
    },
    /// A non-freeform tool item was encountered.
    #[error("unsupported non-freeform tool item type `{item_type}`")]
    UnsupportedNonFreeformToolItem {
        /// Unsupported output-item type.
        item_type: String,
    },
    /// A non-freeform stream event was encountered.
    #[error("unsupported non-freeform stream event type `{event_type}`")]
    UnsupportedNonFreeformStreamEvent {
        /// Unsupported stream event type.
        event_type: String,
    },
}

/// Parse a final Responses output item as a freeform/custom tool call.
///
pub fn parse_freeform_tool_call(item: &Value) -> Result<Option<FreeformToolCall>, NativeToolError> {
    let object = item.as_object().ok_or(NativeToolError::ItemNotObject)?;
    let Some(item_type) = object.get(&"type").and_then(JsonValueTrait::as_str) else {
        return Ok(None);
    };

    match item_type {
        FREEFORM_TOOL_CALL_TYPE => Ok(Some(FreeformToolCall {
            call_id: required_string(object.get(&"call_id"), "call_id")?.to_string(),
            name: required_string(object.get(&"name"), "name")?.to_string(),
            input: required_string(object.get(&"input"), "input")?.to_string(),
        })),
        "local_shell_call" | "tool_call" | "tool_search_call" => {
            Err(NativeToolError::UnsupportedNonFreeformToolItem {
                item_type: item_type.to_string(),
            })
        }
        _ => Ok(None),
    }
}

/// Parse a final Responses output item as a JSON/function tool call.
pub fn parse_function_tool_call(item: &Value) -> Result<Option<FunctionToolCall>, NativeToolError> {
    let object = item.as_object().ok_or(NativeToolError::ItemNotObject)?;
    let Some(item_type) = object.get(&"type").and_then(JsonValueTrait::as_str) else {
        return Ok(None);
    };

    match item_type {
        FUNCTION_TOOL_CALL_TYPE => Ok(Some(FunctionToolCall {
            call_id: required_string(object.get(&"call_id"), "call_id")?.to_string(),
            name: required_string(object.get(&"name"), "name")?.to_string(),
            arguments: required_string(object.get(&"arguments"), "arguments")?.to_string(),
        })),
        _ => Ok(None),
    }
}

/// Parse a Responses stream frame containing a final freeform tool call item.
///
/// For WebSocket streams these are plain JSON frames; for SSE streams Codex
/// treats `response.output_item.done` as the completed output item. This parser
/// waits for that final `item` payload and then applies
/// [`parse_freeform_tool_call`].
pub fn parse_freeform_tool_call_from_frame(
    frame: &Value,
) -> Result<Option<FreeformToolCall>, NativeToolError> {
    let object = frame.as_object().ok_or(NativeToolError::ItemNotObject)?;
    let Some(frame_type) = object.get(&"type").and_then(JsonValueTrait::as_str) else {
        return Ok(None);
    };

    match frame_type {
        RESPONSE_OUTPUT_ITEM_DONE_TYPE => {
            let item = object
                .get(&"item")
                .ok_or(NativeToolError::MissingJsonField { field: "item" })?;
            parse_freeform_tool_call(item)
        }
        _ => Ok(None),
    }
}

/// Parse a Responses stream frame containing a final JSON/function tool call item.
pub fn parse_function_tool_call_from_frame(
    frame: &Value,
) -> Result<Option<FunctionToolCall>, NativeToolError> {
    let object = frame.as_object().ok_or(NativeToolError::ItemNotObject)?;
    let Some(frame_type) = object.get(&"type").and_then(JsonValueTrait::as_str) else {
        return Ok(None);
    };

    match frame_type {
        RESPONSE_OUTPUT_ITEM_DONE_TYPE => {
            let item = object
                .get(&"item")
                .ok_or(NativeToolError::MissingJsonField { field: "item" })?;
            parse_function_tool_call(item)
        }
        _ => Ok(None),
    }
}

/// Parse a freeform/custom tool input delta frame.
///
/// This handles `response.custom_tool_call_input.delta`, the stream event used
/// by Responses custom tools to send raw freeform input before the final
/// `custom_tool_call` item is done.
pub fn parse_freeform_tool_input_delta_from_frame(
    frame: &Value,
) -> Result<Option<FreeformToolInputDelta>, NativeToolError> {
    let object = frame.as_object().ok_or(NativeToolError::ItemNotObject)?;
    let Some(frame_type) = object.get(&"type").and_then(JsonValueTrait::as_str) else {
        return Ok(None);
    };

    match frame_type {
        RESPONSE_CUSTOM_TOOL_CALL_INPUT_DELTA_TYPE => {
            let call_id = object
                .get(&"call_id")
                .and_then(JsonValueTrait::as_str)
                .map(str::to_string);
            let item_id = object
                .get(&"item_id")
                .and_then(JsonValueTrait::as_str)
                .map(str::to_string)
                .or_else(|| call_id.clone())
                .ok_or(NativeToolError::MissingStringField { field: "item_id" })?;
            let delta = required_string(object.get(&"delta"), "delta")?.to_string();
            Ok(Some(FreeformToolInputDelta {
                item_id,
                call_id,
                delta,
            }))
        }
        _ => Ok(None),
    }
}

/// Parse a JSON/function tool argument delta frame.
pub fn parse_function_tool_input_delta_from_frame(
    frame: &Value,
) -> Result<Option<FunctionToolInputDelta>, NativeToolError> {
    let object = frame.as_object().ok_or(NativeToolError::ItemNotObject)?;
    let Some(frame_type) = object.get(&"type").and_then(JsonValueTrait::as_str) else {
        return Ok(None);
    };

    match frame_type {
        RESPONSE_FUNCTION_CALL_ARGUMENTS_DELTA_TYPE => {
            let call_id = object
                .get(&"call_id")
                .and_then(JsonValueTrait::as_str)
                .map(str::to_string);
            let item_id = object
                .get(&"item_id")
                .and_then(JsonValueTrait::as_str)
                .map(str::to_string)
                .or_else(|| call_id.clone())
                .ok_or(NativeToolError::MissingStringField { field: "item_id" })?;
            let delta = required_string(object.get(&"delta"), "delta")?.to_string();
            Ok(Some(FunctionToolInputDelta {
                item_id,
                call_id,
                delta,
            }))
        }
        _ => Ok(None),
    }
}

fn required_string<'a>(
    value: Option<&'a Value>,
    field: &'static str,
) -> Result<&'a str, NativeToolError> {
    value
        .and_then(JsonValueTrait::as_str)
        .ok_or(NativeToolError::MissingStringField { field })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FreeformToolOutputItem<'a> {
    #[serde(rename = "type")]
    item_type: &'static str,
    call_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    output: &'a str,
}

/// Build an unnamed Responses input item that reports a freeform tool result.
///
/// Use [`named_freeform_tool_output_item`] when the result should carry the
/// tool name as well as the call id.
pub fn freeform_tool_output_item(call_id: &str, output: &str) -> Result<Value, sonic_rs::Error> {
    named_freeform_tool_output_item(call_id, None, output)
}

/// Build the Responses input item that reports a freeform tool result.
///
/// The wire shape is `type: "custom_tool_call_output"`. Function-call output
/// items are intentionally not produced by this helper.
pub fn named_freeform_tool_output_item(
    call_id: &str,
    name: Option<&str>,
    output: &str,
) -> Result<Value, sonic_rs::Error> {
    sonic_rs::to_value(&FreeformToolOutputItem {
        item_type: FREEFORM_TOOL_CALL_OUTPUT_TYPE,
        call_id,
        name,
        output,
    })
}

#[cfg(test)]
mod tests {
    use sonic_rs::{JsonContainerTrait, json};

    use super::*;

    #[test]
    fn apply_patch_freeform_tool_serializes_codex_custom_shape() {
        let tools = create_tools_json_for_responses_api(&[NativeTool::apply_patch()]).unwrap();

        assert_eq!(
            tools[0]
                .as_object()
                .unwrap()
                .get(&"type")
                .and_then(JsonValueTrait::as_str),
            Some(FREEFORM_TOOL_TYPE)
        );
        assert_eq!(
            tools[0]
                .as_object()
                .unwrap()
                .get(&"name")
                .and_then(JsonValueTrait::as_str),
            Some(APPLY_PATCH_TOOL_NAME)
        );
        assert_eq!(
            tools[0]
                .as_object()
                .unwrap()
                .get(&"format")
                .and_then(|format| format.as_object())
                .and_then(|format| format.get(&"definition"))
                .and_then(JsonValueTrait::as_str),
            Some(APPLY_PATCH_LARK_GRAMMAR)
        );
    }

    #[test]
    fn terminal_tools_serialize_custom_wire_shape() {
        let tools = create_tools_json_for_responses_api(&[
            NativeTool::terminal_open(),
            NativeTool::terminal_write(),
            NativeTool::terminal_read(),
        ])
        .unwrap();

        let names_and_grammars = [
            (TERMINAL_OPEN_TOOL_NAME, TERMINAL_OPEN_LARK_GRAMMAR),
            (TERMINAL_WRITE_TOOL_NAME, TERMINAL_WRITE_LARK_GRAMMAR),
            (TERMINAL_READ_TOOL_NAME, TERMINAL_READ_LARK_GRAMMAR),
        ];
        for (tool, (name, grammar)) in tools.iter().zip(names_and_grammars) {
            let object = tool.as_object().unwrap();
            assert_eq!(
                object.get(&"type").and_then(JsonValueTrait::as_str),
                Some(FREEFORM_TOOL_TYPE)
            );
            assert_eq!(
                object.get(&"name").and_then(JsonValueTrait::as_str),
                Some(name)
            );
            assert!(object.get(&"parameters").is_none());
            assert!(object.get(&"strict").is_none());
            assert_eq!(
                object
                    .get(&"format")
                    .and_then(|value| value.as_object())
                    .and_then(|format| format.get(&"definition"))
                    .and_then(JsonValueTrait::as_str),
                Some(grammar)
            );
        }
    }

    #[test]
    fn inspect_tool_serializes_custom_tool_shape() {
        let tools = create_tools_json_for_responses_api(&[NativeTool::inspect()]).unwrap();
        let object = tools[0].as_object().unwrap();
        assert_eq!(
            object.get(&"type").and_then(JsonValueTrait::as_str),
            Some(FREEFORM_TOOL_TYPE)
        );
        assert_eq!(
            object.get(&"name").and_then(JsonValueTrait::as_str),
            Some(INSPECT_TOOL_NAME)
        );
        assert_eq!(
            object
                .get(&"format")
                .and_then(|value| value.as_object())
                .and_then(|format| format.get(&"definition"))
                .and_then(JsonValueTrait::as_str),
            Some(INSPECT_LARK_GRAMMAR)
        );
    }

    #[test]
    fn codex_registry_advertises_inspect_edit_and_terminal_handlers() {
        let registry = NativeToolRegistry::codex();
        let expected_names = vec![
            LOCATE_TOOL_NAME.to_string(),
            INSPECT_TOOL_NAME.to_string(),
            EDIT_FILE_TOOL_NAME.to_string(),
            TERMINAL_OPEN_TOOL_NAME.to_string(),
            TERMINAL_WRITE_TOOL_NAME.to_string(),
            TERMINAL_READ_TOOL_NAME.to_string(),
            MARK_TASK_COMPLETE_TOOL_NAME.to_string(),
        ];
        assert_eq!(registry.tool_names(), expected_names);
        assert_eq!(
            registry.handler_for(TERMINAL_OPEN_TOOL_NAME),
            Some(NativeToolHandler::Terminal)
        );
        assert_eq!(registry.handler_for(APPLY_PATCH_TOOL_NAME), None);
        assert_eq!(
            registry.handler_for(EDIT_FILE_TOOL_NAME),
            Some(NativeToolHandler::EditFile)
        );
        assert_eq!(
            registry.handler_for(LOCATE_TOOL_NAME),
            Some(NativeToolHandler::Locate)
        );
        assert_eq!(
            registry.handler_for(INSPECT_TOOL_NAME),
            Some(NativeToolHandler::Inspect)
        );
        assert_eq!(
            registry.handler_for(MARK_TASK_COMPLETE_TOOL_NAME),
            Some(NativeToolHandler::MarkTaskComplete)
        );
        assert_eq!(registry.handler_for(STAGED_PATCH_APPLY_TOOL_NAME), None);
        assert_eq!(registry.handler_for(STAGED_PATCH_DISCARD_TOOL_NAME), None);
    }

    #[test]
    fn mark_task_complete_serializes_custom_tool_shape() {
        let tools =
            create_tools_json_for_responses_api(&[NativeTool::mark_task_complete()]).unwrap();

        assert_eq!(
            tools[0]
                .as_object()
                .unwrap()
                .get(&"type")
                .and_then(JsonValueTrait::as_str),
            Some(FREEFORM_TOOL_TYPE)
        );
        assert_eq!(
            tools[0]
                .as_object()
                .unwrap()
                .get(&"name")
                .and_then(JsonValueTrait::as_str),
            Some(MARK_TASK_COMPLETE_TOOL_NAME)
        );
        assert_eq!(
            tools[0]
                .as_object()
                .unwrap()
                .get(&"format")
                .and_then(|format| format.as_object())
                .and_then(|format| format.get(&"definition"))
                .and_then(JsonValueTrait::as_str),
            Some(MARK_TASK_COMPLETE_LARK_GRAMMAR)
        );
    }
    #[test]
    fn ollama_cloud_registry_wraps_freeform_tools_as_functions() {
        let registry = NativeToolRegistry::ollama_cloud();
        let tools = registry.tools();
        let names = tools.iter().map(NativeTool::name).collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                LOCATE_TOOL_NAME,
                INSPECT_TOOL_NAME,
                EDIT_FILE_TOOL_NAME,
                TERMINAL_OPEN_TOOL_NAME,
                TERMINAL_WRITE_TOOL_NAME,
                TERMINAL_READ_TOOL_NAME,
                MARK_TASK_COMPLETE_TOOL_NAME,
            ]
        );

        let values = create_tools_json_for_responses_api(&tools).unwrap();
        for object in values.iter().map(|value| value.as_object().unwrap()) {
            assert_eq!(
                object.get(&"type").and_then(JsonValueTrait::as_str),
                Some(FUNCTION_TOOL_TYPE)
            );
            assert_eq!(
                object.get(&"strict").and_then(JsonValueTrait::as_bool),
                Some(true)
            );
            assert!(object.get(&"parameters").is_some());
            assert!(object.get(&"format").is_none());

            let description = object
                .get(&"description")
                .and_then(JsonValueTrait::as_str)
                .unwrap();
            assert!(
                description.contains("single JSON string"),
                "wrapper description should make the JSON/native boundary explicit: {description}"
            );
            assert!(
                description.contains("Native tool instructions:")
                    && !description.ends_with("Native tool instructions:"),
                "wrapper description should preserve the native tool instructions: {description}"
            );

            let parameters = object.get(&"parameters").unwrap();
            let input_description = parameters
                .get(&"properties")
                .and_then(|properties| properties.get(&"input"))
                .and_then(|input| input.get(&"description"))
                .and_then(JsonValueTrait::as_str)
                .unwrap();
            assert!(
                input_description.contains("literal newlines")
                    && input_description.contains("input:"),
                "input property description should explain how to pass block-style raw input: {input_description}"
            );
            assert!(
                input_description.contains("Native tool instructions:")
                    && !input_description.ends_with("Native tool instructions:"),
                "input property description should preserve the native tool instructions: {input_description}"
            );
        }

        assert_eq!(
            parse_function_wrapped_freeform_input(r#"{"input":"pwd"}"#).unwrap(),
            "pwd"
        );
    }

    #[test]
    fn parse_function_wrapped_freeform_input_rejects_invalid_arguments() {
        assert!(parse_function_wrapped_freeform_input("{}").is_err());
    }

    #[test]
    fn without_terminal_tools_drops_terminal_entries() {
        let registry = NativeToolRegistry::codex();
        assert!(registry.has_terminal_tools());

        let filtered = registry.without_terminal_tools();
        assert!(!filtered.has_terminal_tools());
        let names = filtered.tool_names();
        assert!(!names.iter().any(|name| name == TERMINAL_OPEN_TOOL_NAME));
        assert!(!names.iter().any(|name| name == TERMINAL_WRITE_TOOL_NAME));
        assert!(!names.iter().any(|name| name == TERMINAL_READ_TOOL_NAME));
        assert!(filtered.handler_for(TERMINAL_OPEN_TOOL_NAME).is_none());
    }

    #[test]
    fn parse_freeform_tool_call_accepts_only_custom_tool_calls() {
        let call = parse_freeform_tool_call(&json!({
            "type": "custom_tool_call",
            "call_id": "call-1",
            "name": "apply_patch",
            "input": "*** Begin Patch\n*** End Patch\n",
        }))
        .unwrap()
        .unwrap();

        assert_eq!(
            call,
            FreeformToolCall {
                call_id: "call-1".to_string(),
                name: "apply_patch".to_string(),
                input: "*** Begin Patch\n*** End Patch\n".to_string(),
            }
        );
    }

    #[test]
    fn parse_freeform_tool_call_ignores_function_call_items() {
        let call = parse_freeform_tool_call(&json!({
            "type": "function_call",
            "call_id": "call-legacy",
            "name": "apply_patch",
            "arguments": "{}",
        }))
        .unwrap();

        assert_eq!(call, None);
    }

    #[test]
    fn parse_function_tool_call_reads_function_call_items() {
        let call = parse_function_tool_call(&json!({
            "type": "function_call",
            "call_id": "call-function",
            "name": "function_tool",
            "arguments": "{\"cmd\":\"date\"}",
        }))
        .unwrap()
        .unwrap();

        assert_eq!(
            call,
            FunctionToolCall {
                call_id: "call-function".to_string(),
                name: "function_tool".to_string(),
                arguments: "{\"cmd\":\"date\"}".to_string(),
            }
        );
    }

    #[test]
    fn parse_freeform_tool_call_from_frame_reads_output_items() {
        let call = parse_freeform_tool_call_from_frame(&json!({
            "type": "response.output_item.done",
            "item": {
                "type": "custom_tool_call",
                "call_id": "call-frame",
                "name": "apply_patch",
                "input": "*** Begin Patch\n*** End Patch\n"
            }
        }))
        .unwrap()
        .unwrap();

        assert_eq!(call.call_id, "call-frame");
        assert_eq!(call.name, "apply_patch");
    }

    #[test]
    fn parse_freeform_tool_call_from_frame_waits_for_done_item() {
        let call = parse_freeform_tool_call_from_frame(&json!({
            "type": "response.output_item.added",
            "item": {
                "type": "custom_tool_call",
                "call_id": "call-frame",
                "name": "apply_patch"
            }
        }))
        .unwrap();

        assert_eq!(call, None);
    }

    #[test]
    fn parse_freeform_tool_call_from_frame_ignores_function_output_items() {
        let call = parse_freeform_tool_call_from_frame(&json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "call_id": "call-function",
                "name": "function_tool",
                "arguments": "{}"
            }
        }))
        .unwrap();

        assert_eq!(call, None);
    }

    #[test]
    fn parse_function_tool_call_from_frame_reads_output_items() {
        let call = parse_function_tool_call_from_frame(&json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "call_id": "call-function",
                "name": "function_tool",
                "arguments": "{\"cmd\":\"date\"}"
            }
        }))
        .unwrap()
        .unwrap();

        assert_eq!(call.call_id, "call-function");
        assert_eq!(call.name, "function_tool");
        assert_eq!(call.arguments, "{\"cmd\":\"date\"}");
    }

    #[test]
    fn parse_freeform_tool_input_delta_reads_custom_delta_frames() {
        let delta = parse_freeform_tool_input_delta_from_frame(&json!({
            "type": "response.custom_tool_call_input.delta",
            "item_id": "ctc-1",
            "call_id": "call-1",
            "delta": "*** Begin Patch\n",
        }))
        .unwrap()
        .unwrap();

        assert_eq!(
            delta,
            FreeformToolInputDelta {
                item_id: "ctc-1".to_string(),
                call_id: Some("call-1".to_string()),
                delta: "*** Begin Patch\n".to_string(),
            }
        );
    }

    #[test]
    fn parse_freeform_tool_input_delta_uses_call_id_as_item_id_when_needed() {
        let delta = parse_freeform_tool_input_delta_from_frame(&json!({
            "type": "response.custom_tool_call_input.delta",
            "call_id": "call-1",
            "delta": "*** Begin Patch\n",
        }))
        .unwrap()
        .unwrap();

        assert_eq!(delta.item_id, "call-1");
        assert_eq!(delta.call_id.as_deref(), Some("call-1"));
    }

    #[test]
    fn parse_freeform_tool_input_delta_ignores_function_argument_delta() {
        let delta = parse_freeform_tool_input_delta_from_frame(&json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc-1",
            "delta": "{\"cmd\":\"",
        }))
        .unwrap();

        assert_eq!(delta, None);
    }

    #[test]
    fn parse_function_tool_input_delta_reads_function_argument_delta() {
        let delta = parse_function_tool_input_delta_from_frame(&json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc-1",
            "call_id": "call-function",
            "delta": "{\"cmd\":\"",
        }))
        .unwrap()
        .unwrap();

        assert_eq!(
            delta,
            FunctionToolInputDelta {
                item_id: "fc-1".to_string(),
                call_id: Some("call-function".to_string()),
                delta: "{\"cmd\":\"".to_string(),
            }
        );
    }

    #[test]
    fn freeform_tool_output_item_serializes_custom_tool_output() {
        let output = named_freeform_tool_output_item("call-1", Some("apply_patch"), "Done")
            .expect("serialize output");

        assert_eq!(
            output,
            json!({
                "type": "custom_tool_call_output",
                "call_id": "call-1",
                "name": "apply_patch",
                "output": "Done",
            })
        );
    }

    #[test]
    fn freeform_tool_specs_do_not_emit_function_or_namespace_tools() {
        let tools = create_tools_json_for_responses_api(&[NativeTool::apply_patch()]).unwrap();
        for tool in tools {
            let tool_type = tool
                .as_object()
                .and_then(|object| object.get(&"type"))
                .and_then(JsonValueTrait::as_str)
                .unwrap();
            assert_eq!(tool_type, FREEFORM_TOOL_TYPE);
            assert_ne!(tool_type, "function");
            assert_ne!(tool_type, "namespace");
        }
    }
}
