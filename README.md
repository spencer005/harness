# DONT USE THIS YET

# DONT USE THIS YET

It works for me but holy shit, this codebase is a mess and needs to be completely
redone in many ways like there's some good ideas but the llms of today are too
stupid to figure out what I meant so I need to spoonfeed my ideas in isolated
pieces for it to actually be correct but for now its just some shit I made to
avoid using codex since codex is EVEN WORSE SOMEHOW probably becasue they are
also vibecoding but that shit is just diabolical using 500mb of memory and
loading the entire transcript in memory, database contention, completely failing
if you open multiple windows. I prefer my shit having a bunch of random bugs and
having a bunch of broken visuals and bad code to causing oom killer to activate.

## CODEBASE SLOP RATIO: 75%

# new_harness

Minimal Codex-style harness target:

- Ratatui UI.
- Responses API over WebSocket.
- WebSocket connections are pooled and kept open instead of repeatedly reconnecting.
- New WebSocket handshakes are rate-limited to 60 per minute.
- Native in-process subagents.
- Native Responses tool specs. The CLI advertises freeform `locate`, `read_file`, `edit_file`,
  `terminal_open`, `terminal_write`, and `terminal_read`; root turns persist tool outputs before
  continuation requests.
- `locate`: get context. If know nothing, ask
  directly: `need context for [your task]`. If enough known, ask specific: `how thing1 implemented`,
  `what is /path/file`, `all data structures for feature`, or `how edit_file implemented`.
  Locator tool no omniscient; pass semantically required info for good result. Get context only; do
  not ask locator to make decisions. `read_file` emits compact line-boundary anchors. `edit_file`
  applies native anchor-based edits directly.
- Manual compaction.
- Steering prompts: queue for the next tool boundary, or interrupt and send immediately.

This workspace intentionally excludes app-server, MCP/apps/connectors, plugin/skill systems, cloud
tasks, memories, external-agent migration, and provider-specific local model integrations.

## Crates

- `harness-core`: harness orchestration, session storage, Responses WS pool contract, subagents,
  tools, compact, steering.
- `harness-tui`: Ratatui rendering/input surface.
- `harness-cli`: binary entrypoint.

## Runtime wiring

The TUI sends input to the harness actor. The harness actor persists turn records through the
session actor before submitting a Responses WebSocket request. The Responses actor owns transport
and streams parsed events back to the harness, which updates the session and emits render events.

```text
TUI -> HarnessActor -> SessionActor
                  \-> ResponsesWsActor
```

Session files are binary `.nhsession` files: typed serde records encoded with `postcard`, grouped
into append chunks, and compressed with zstd. `session_index.log` and `session_index.snapshot` keep
id/title lookup fast without scanning session blobs.

CLI-created root session/thread ids are UUID-shaped strings, while durable session lookup still uses
the local binary session index.

WebSocket sockets themselves are not durable. The durable WS pool state stores only the next
allowed new-connection timestamp. Normal operation reuses completed idle sockets within the same
provider/auth pool; request-specific Codex headers do not partition idle sockets. Current per-frame
metadata such as window id, parent thread id, subagent, installation id, and turn metadata is
stamped into each WebSocket request body. Cold handshakes are spaced by 1500ms by default.
Automatic startup prewarm is not wired by default; sockets are opened on demand unless a caller
explicitly sends a prewarm request.

## Running

Required:

- Harness ChatGPT auth state at `$XDG_STATE_HOME/new_harness/auth.json`, or
  `$HOME/.local/state/new_harness/auth.json` when `XDG_STATE_HOME` is unset. If the harness state
  file does not exist, the CLI imports the current Codex access token from `$CODEX_HOME/auth.json`
  or `$HOME/.codex/auth.json`.
- Harness base instructions at `$XDG_STATE_HOME/new_harness/base-instructions.md`, or
  `$HOME/.local/state/new_harness/base-instructions.md` when `XDG_STATE_HOME` is unset. If the
  harness instructions file does not exist, the CLI imports the current model instructions from
  `$CODEX_HOME/<model>-model-instructions.md`.

Optional:

- `CODEX_HOME` selects the Codex auth directory.
- `XDG_STATE_HOME` selects the harness state directory.
- `HARNESS_MODEL` defaults to `gpt-5.5`.
- `HARNESS_REASONING_EFFORT` defaults to `xhigh`.
- `HARNESS_SERVICE_TIER` defaults to `fast` (`priority` on the wire).
- TUI input defaults to the Responses `developer` role. Use `/developer off` to send subsequent
  input as the `user` role, and `/developer on` to switch back to `developer`.
- `OPENAI_BASE_URL` defaults to `https://chatgpt.com/backend-api/codex`.
- `HARNESS_SESSION_ROOT` defaults to `$HOME/.new_harness`.
- `--norotate` bypasses harness state and reads Codex `auth.json` only. Without `--norotate`, token
  refresh is only attempted when the harness state file contains a harness-owned refresh token; the
  default Codex import does not copy Codex's refresh token.

Example:

```sh
cargo run -p harness-cli -- --norotate
```

Phase 9 tool-shape experiment:

```sh
cargo run -p harness-cli -- experiment freeform-parallel --norotate
```

The experiment command uses the same ChatGPT/Codex auth path as the TUI, queries `/models`, then
runs live Responses WebSocket requests for two controlled custom/freeform tools and two comparable
function tools. It prints model metadata, the request `parallel_tool_calls` value, sent tool specs,
event order/item ids, inferred call overlap or serialization, continuation behavior, latency, and
token usage when the backend returns usage.

WebSocket reuse metadata probe:

```sh
cargo run -p harness-cli -- experiment ws-reuse-metadata --norotate
```

This live probe allows only one new WebSocket connection per minute, sends one request, then sends a
second request with changed logical session/thread/window/parent/subagent/turn metadata. If the
second request completes, those request-header changes did not require another handshake.

In the TUI, `/model 5.5 xhigh fast` selects `gpt-5.5`, extra-high reasoning, and Codex Fast mode.
The bottom status line shows the session id, model/reasoning/tier, `role: developer` or
`role: user`, live stream state, and TTFT for the most recent response.

## Current TUI controls

- Type text to edit the input line.
- `Enter` submits the input to the harness and clears the input. While a response is active,
  `Enter` queues the input as steering instead of submitting it immediately, except slash commands
  beginning with `/` are sent to the runtime immediately.
- `Ctrl-J` or `Shift-Enter` inserts a newline in the input editor; terminal-reported
  `Shift-Enter` repeats insert repeated newlines.
- The input area starts at 3 rows and grows with multiline input up to 40% of the terminal height.
- The input area has a 2-column left margin with a guide line, prompt (`›`), and
  `▲`/`▼` overflow indicators.
- `Ctrl-A` and `Ctrl-E` move to the start/end of the current input line.
- `Left`, `Right`, `Home`, and `End` move the input cursor.
- `Ctrl-Backspace` or `Alt-Backspace` deletes the previous word.
- `Ctrl-Z` undoes the previous input edit; `Ctrl-Y` redoes it.
- `Ctrl-C` clears non-empty input. With empty input, the first `Ctrl-C` shows a yellow warning on
  the status line, and the second `Ctrl-C` exits.
- While a response is active, the unframed activity strip above the input shows
  `• Working (<elapsed>s • esc to interrupt)`. `Esc` interrupts instead of exiting in that state.
- Queued steering is shown in the same activity strip as
  `queued <message> (esc to steer instantly)`. `Esc` sends that queued steer immediately.
- When agents are present, the same unframed strip lists them without a frame; if the strip has more
  agents than available rows it ends with `and x more`.
- Assistant text streams into the transcript as `response.output_text.delta` frames arrive.
- Transcript entries use compact muted markers (`•` for assistant, `»` for input), subtle
  assistant/input text shades, and wrapped long lines; tool blocks keep their syntax highlighting
  behind a compact tool marker.
- `PageUp`/`PageDown` or the mouse wheel scroll the transcript without editing input; `Ctrl-Home`
  jumps to the oldest retained transcript line and `Ctrl-End` returns to live tail follow mode.
  New transcript output autoscrolls while the view is at the tail and preserves the current view
  while scrolled up. A vertical scrollbar is shown when retained transcript content exceeds the
  visible transcript area.
- The TUI renders only the visible transcript viewport and keeps a bounded in-memory transcript tail
  instead of retaining the entire session log. Durable session history remains in the binary session
  store.
- The bottom status line marks an active stream with `● active` and shows `ttft: pending` until the
  first assistant text delta, then displays the measured TTFT in milliseconds.
- `/developer on` sends subsequent input as Responses `developer` messages. This is the default.
- `/developer off` sends subsequent input as Responses `user` messages.
- `/developer` toggles between developer and user input roles.
- `/model <model> [reasoning] [tier]` updates the model settings for subsequent turns.
- `/persist [task|pause|continue]` toggles or controls automatic continuation for the previous or explicitly provided task
  until the model verifies completion criteria and calls the `mark_task_complete` custom tool. Run
  `/persist` again with no task to disable active persist mode. Use `/persist pause` to pause the loop without ending the current request, `/persist continue` to resume, or send another message/steering to resume. If a user interrupt is requested (e.g. via `Esc`), the persist loop is automatically paused.
- `Backspace` deletes the previous input character.
- Idle `Esc` or confirmed `Ctrl-C` exits the TUI and prints the session id for resume.
