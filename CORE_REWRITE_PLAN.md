# Core rewrite plan

The rewrite should not produce another monolithic `harness-core`. The current crate combines at least six systems with different ownership, durability, security, and lifecycle requirements:

1. conversation orchestration;
2. model/provider transport;
3. durable session storage;
4. local tool execution;
5. frontend/runtime protocol;
6. session query/IPC.

The replacement should separate those systems and make `harness-conversation-runtime` the composition layer—not another container for their implementations.
Migration constraint: `.nhsession` format redesign is deferred until the final phase. Earlier phases do not rewrite, migrate, or silently reinterpret existing session files. They may define and test the session-store boundary against the current format, but any on-disk format change is a separately supervised operation that the user runs explicitly. No automatic startup migration or runtime fallback is part of the design.


## Target bounded contexts

### `harness-tool-api`

Owns provider-independent tool contracts only:

```rust
ToolName
ToolDefinition
ToolInputSchema
ToolCallId
ToolCall
ToolInput
ToolResult
ToolPresentation
ToolFailure
ToolCapabilities
ToolRegistry
```

Key distinctions:

```rust
enum ToolInput {
    Freeform(String),
    FunctionJson(String),
}

enum ToolInputSchema {
    FreeformGrammar {
        syntax: GrammarSyntax,
        definition: String,
    },
    JsonSchema(JsonSchema),
}
```

`ToolResult` keeps separate:

- model-visible output;
- frontend presentation;
- artifacts/metadata.

It does not depend on:

- Responses wire types;
- session storage;
- filesystem code;
- Ratatui;
- provider profiles.

Registry construction rejects duplicate names and requires every advertised tool to have an executor route.

### `harness-model-api`

Owns provider-neutral model contracts:

```rust
ModelRequestId
ModelAttemptId
ModelSelection
ModelCapabilities
ContextLimits
ModelInput
ModelRequest
ModelEvent
ModelUsage
ModelFailure
ModelTerminalOutcome
ModelTransport
ProviderId
ResolvedModelRoute
```

Terminal outcomes:

```rust
enum ModelTerminalOutcome {
    Completed(ModelCompletion),
    Interrupted(ModelInterruption),
    Cancelled(ModelCancellation),
    Failed(ModelFailure),
}
```

`ModelRequest` contains canonical model inputs and tool definitions. It does not contain raw `sonic_rs::Value`, HTTP headers, SSE frames, or WebSocket frames.

Every retry carries:

- the same immutable semantic request snapshot;
- a new `ModelAttemptId`;
- an explicit retry reason.

### `harness-runtime-api`

Owns only the frontend/runtime boundary:

```rust
RuntimeCommand
RuntimeEvent
RuntimeSnapshot
RuntimeEventEnvelope
TranscriptSnapshotEntry
TranscriptPayload
ProviderSummary
ModelSummary
AgentSummary
Activity
ActivityStatus
RuntimeCommandSender
RuntimeEventReceiver
```

Channel wrappers:

```rust
async fn send(&self, command: RuntimeCommand) -> Result<(), RuntimeClosed>;
async fn recv(&mut self) -> Result<RuntimeEventEnvelope, RuntimeClosed>;
```

Proposed commands:

```rust
enum RuntimeCommand {
    SubmitPrompt { text: String },
    QueueSteering { text: String },
    Interrupt { text: String },
    SetModel { selection: ModelSelection },
    LoadOlderTranscript {
        before_sequence: Option<u64>,
    },
    Shutdown,
}
```

Provider switching may be a typed runtime command if retained as a TUI feature. It must not be encoded as an arbitrary transcript string.

Every initial transcript entry carries persisted sequence when one exists.

### `harness-responses-protocol`

Owns pure Responses wire logic:

- request encoding;
- message encoding;
- tool encoding;
- incremental SSE decoding;
- WebSocket frame decoding;
- terminal-event recognition;
- protocol-error classification;
- Codex-specific metadata extraction.

No networking, credentials, file access, or runtime actors.

One frame enters; zero or more typed `ModelEvent`s leave. Orchestration never sees raw protocol JSON.

### `harness-responses-transport`

Owns network lifecycle:

- incremental HTTPS response-body streaming;
- WebSocket connection pooling;
- authentication header application;
- auth refresh coordination;
- request cancellation;
- idle timeout;
- request timeout;
- bounded event delivery;
- task joining during shutdown.

Per-request state:

```text
Created
  -> Connecting
  -> Streaming
  -> Completed | Interrupted | Cancelled | Failed
```

Every request has:

- cancellation token;
- task handle;
- bounded event sink;
- terminal-outcome guarantee.

Exactly one terminal outcome is emitted.

### `harness-provider`

Owns:

- provider profiles;
- provider config store;
- credential resolution;
- model catalogs;
- capability validation;
- explicit route construction;
- provider-specific telemetry implementation;
- provider switching preparation.

Output:

```rust
struct ResolvedProvider {
    identity: ProviderIdentity,
    selected_model: ModelSelection,
    capabilities: ModelCapabilities,
    context_limits: ContextLimits,
    routes: ProviderRoutes,
}
```

Routes:

```rust
struct ProviderRoutes {
    root: ResolvedModelRoute,
    compaction: ResolvedModelRoute,
    tool_summary: Option<ResolvedModelRoute>,
    locator: Option<ResolvedModelRoute>,
}
```

No route silently falls back.

### `harness-session-store`

Owns:

- durable session schema;
- sequence assignment;
- one-writer leases;
- append receipts;
- durability policy;
- resume folding;
- transcript paging;
- fork/rollback operations;
- catalog/index maintenance;
- corruption inspection and explicit repair;
- bounded blocking I/O.

It owns its provider-binding DTO. It does not depend on `harness-provider`.

### `harness-tool-execution`

Owns:

- inspect;
- edit;
- apply patch;
- staged patch state;
- PTYs;
- process execution;
- filesystem confinement;
- search/binary inspection;
- output normalization.

It implements `harness-tool-api`.

### `harness-conversation-runtime`

Owns conversation policy and nothing below injected ports.

Dependencies:

```text
harness-runtime-api
harness-model-api
harness-tool-api
harness-session-store
```

Injected implementations:

```rust
SessionJournal
ModelRouter
ToolExecutor
AgentSupervisor
TurnTelemetrySink
Clock
IdSource
```

It does not parse Responses JSON, open files directly, load credentials, or own PTY implementation details.


It depends on `harness-session-store`.

Each client request is isolated. A failed request returns an error response and does not terminate the listener.

## Target dependency direction

```text
harness-tool-api

harness-model-api
    -> harness-tool-api

harness-runtime-api
    -> harness-model-api
    -> harness-tool-api

harness-responses-protocol
    -> harness-model-api
    -> harness-tool-api

harness-responses-transport
    -> harness-responses-protocol
    -> harness-model-api

harness-provider
    -> harness-responses-transport
    -> harness-model-api

harness-session-store
    -> harness-model-api
    -> harness-tool-api

harness-tool-execution
    -> harness-tool-api

harness-conversation-runtime
    -> harness-runtime-api
    -> harness-model-api
    -> harness-tool-api
    -> harness-session-store


Consumers after cutover:

```text
harness-cli
    -> harness-provider
    -> harness-session-store
    -> harness-tool-execution
    -> harness-conversation-runtime
    -> harness-tui-rewrite

harness-tui-rewrite
    -> harness-runtime-api

harness-web
    -> harness-session-store
```

Forbidden dependencies:

- session store → provider implementation;
- model API → Responses protocol;
- tool API → tool execution;
- runtime API → conversation runtime;
- provider → conversation runtime;
- TUI → session store/provider/tool execution.

## Conversation runtime lifecycle

```text
Constructed
  -> Starting
  -> Ready
  -> ShuttingDown
  -> Stopped
```

Startup succeeds only after:

- session state is loaded or new-session intent prepared;
- provider routes resolved;
- tool registry valid;
- model transport supervisor running;
- frontend snapshot constructible.

A startup failure never leaves a partially running runtime handle.

## Root conversation state

```text
Idle
  -> PersistingInput
  -> PreparingAttempt
  -> AwaitingModel
  -> Streaming
  -> PersistingAssistant
  -> PersistingToolCall
  -> ExecutingTool
  -> PersistingToolResult
  -> PreparingContinuation
  -> Idle
```

Additional branches:

```text
PreparingAttempt
  -> Compacting
  -> PreparingAttempt

AwaitingModel | Streaming
  -> Cancelling
  -> Interrupted
  -> PersistingAssistant
  -> PreparingContinuation | Idle

Any active phase
  -> Failed
  -> Idle
```

These phases are mutually exclusive. There is no separate ignored-request set.

## Active model attempt

```rust
struct ActiveModelAttempt {
    turn_id: TurnId,
    attempt_id: ModelAttemptId,
    request: Arc<ModelRequest>,
    route: ResolvedModelRoute,
    phase: ModelAttemptPhase,
    assistant: StreamingAssistant,
    pending_tool_inputs: ToolInputAssembly,
}
```

The immutable request captures:

- exact history revision;
- provider route;
- model settings;
- instructions;
- tool definitions;
- steering included in this attempt.

Reconnect retry uses the same `Arc<ModelRequest>`. Rebuilding from newer history creates a new semantic request.

## Supervised jobs

```rust
enum JobPurpose {
    RootModelAttempt { attempt_id: ModelAttemptId },
    Compaction { compaction_id: CompactionId },
    ToolExecution { execution_id: ToolExecutionId },
    ProviderResolution { operation_id: OperationId },
    AgentRun { agent_id: AgentId },
    Telemetry { event_id: TelemetryEventId },
}
```

Supervisor retains:

- purpose;
- cancellation token;
- join handle;
- expected completion type.

Every completion returns through runtime loop with typed ID. Unknown or duplicate completions are protocol violations. No detached tasks remain.

## Runtime loop

The runtime loop does not await long I/O inline.

It:

1. receives frontend command, model event, job completion, or shutdown deadline;
2. applies one state transition;
3. schedules typed effects;
4. emits ordered frontend events;
5. continues accepting cancellation and shutdown commands.

Session append, provider resolution, tool execution, and model jobs run outside reducer and return typed completions.

## Durable conversation rules

Canonical history consists only of durably acknowledged records.

Transient state includes:

- streamed assistant text;
- partial tool inputs;
- pending model jobs;
- frontend notices.

Transient state may be rendered but cannot build a continuation until required semantic records are durable.

Input transaction:

```text
SubmitPrompt
  -> validate exact source
  -> ensure/open session writer
  -> append TurnStarted + InputMessage durably
  -> update canonical in-memory history
  -> emit transcript event
  -> prepare model attempt
```

If append fails:

- canonical history unchanged;
- no model request starts;
- frontend receives typed failure;
- prompt ownership follows explicit command acceptance semantics.

Runtime must not trim source text incidentally.

Assistant transaction:

```text
transient assistant text
  -> durable AssistantMessage/AssistantPartial record
  -> canonical history update
  -> frontend commit event
```

If persistence fails, no continuation starts.

Tool transaction:

```text
ToolCall received
  -> durable ToolCallAccepted
  -> durable ToolExecutionStarted
  -> execute tool
  -> durable ToolExecutionFinished
  -> update canonical history
  -> emit frontend result
  -> start continuation
```

Crash after `ToolExecutionStarted` but before finish produces unresolved execution. Read-only idempotent tools may declare safe retry. Side-effecting tools become `OutcomeUnknownAfterRestart`; no blind duplicate execution.

Turn outcome:

```rust
enum TurnOutcome {
    Completed,
    Interrupted { reason: InterruptReason },
    Cancelled { reason: CancellationReason },
    Failed { failure: TurnFailure },
}
```

Transport interruption is never completion.

## Session-store design

Owns:

- durable session schema;
- sequence assignment;
- one-writer leases;
- append receipts;
- durability policy;
- resume folding;
- transcript paging;
- fork/rollback operations;
- catalog/index maintenance;
- corruption inspection and explicit repair;
- bounded blocking I/O.

It owns its provider-binding DTO. It does not depend on `harness-provider`.

During implementation, the session-store boundary preserves the existing `.nhsession` format. The target strict schema and any replacement on-disk format remain deferred until the final, user-run migration phase.

Target strict schema:

```rust
enum SessionRecord {
    Metadata(SessionMetadata),
    ProviderBinding(SessionProviderBinding),
    TurnStarted(TurnStarted),
    InputMessage(InputMessage),
    ModelAttemptStarted(ModelAttemptStarted),
    AssistantMessage(AssistantMessage),
    AssistantPartial(AssistantPartial),
    ToolCallAccepted(ToolCallAccepted),
    ToolExecutionStarted(ToolExecutionStarted),
    ToolExecutionFinished(ToolExecutionFinished),
    TurnFinished(TurnFinished),
    CompactionCommitted(CompactionCommitted),
    SessionClosed(SessionClosed),
}
```

Transcript is a projection, not the storage schema.

Writer ownership:

- non-cloneable `SessionWriterLease`;
- OS-level lock;
- one lease per session;
- sequence assignment inside lease;
- joined close;
- read handles cannot append.

Append API:

```rust
enum Durability {
    Buffered,
    Durable,
}

struct AppendReceipt {
    sequences: RangeInclusive<SessionSequence>,
    durability: Durability,
}
```

Durable acknowledgement means chunk written, file data synchronized, and writer state advanced consistently.

Target replacement-format requirements:

- magic/version;
- bounded chunk lengths;
- first sequence;
- record count;
- checksum;
- contiguous-sequence validation;
- explicit session identity;
- incomplete-tail detection.

Until the final migration phase, these replacement-format requirements are design targets only. The implementation does not rewrite existing `.nhsession` files to satisfy them.

The target reader has no normal legacy decoding. Existing sessions remain behind the current-format boundary until the explicit migration is run. Any importer is an offline operation, not a normal runtime fallback.

Blocking execution:

- one dedicated blocking writer task per active session;
- bounded read/decompression pool;
- bounded queues;
- no filesystem/compression on async executor threads.

Paging:

```rust
LoadOlderEntries {
    before: Option<SessionSequence>,
    maximum_entries: PageSize,
}
```

Result:

```rust
TranscriptPage {
    entries,
    next_before,
    reached_start,
}
```

No `max_lines`.

Catalog:

- reconstructible;
- atomic snapshot replacement;
- parent-directory fsync;
- compacted journal;
- stale entry removal;
- bounded blocking execution;
- explicit rebuild;
- no ordinary decompression unless verification requested.

Recovery:

- fold strict records;
- verify sequence continuity;
- identify incomplete turns/jobs;
- append explicit recovery outcomes;
- only then expose `Ready`.


Blocking execution:

- one dedicated blocking writer task per active session;
- bounded read/decompression pool;
- bounded queues;
- no filesystem/compression on async executor threads.

Paging:

```rust
LoadOlderEntries {
    before: Option<SessionSequence>,
    maximum_entries: PageSize,
}
```

Result:

```rust
TranscriptPage {
    entries,
    next_before,
    reached_start,
}
```

No `max_lines`.

Catalog:

- reconstructible;
- atomic snapshot replacement;
- parent-directory fsync;
- compacted journal;
- stale entry removal;
- bounded blocking execution;
- explicit rebuild;
- no ordinary decompression unless verification requested.

Recovery:

- fold strict records;
- verify sequence continuity;
- identify incomplete turns/jobs;
- append explicit recovery outcomes;
- only then expose `Ready`.

## Responses protocol and transport

Protocol output:

```rust
enum ModelEvent {
    Started,
    Metadata(ModelResponseMetadata),
    AssistantTextDelta(String),
    ToolInputDelta(ToolInputDelta),
    ToolCall(ToolCall),
    Usage(ModelUsage),
    Terminal(ModelTerminalOutcome),
}
```

Conversation runtime never sees raw JSON.

Incremental SSE carries:

- partial UTF-8;
- partial line;
- multiple `data:` lines;
- comments;
- event termination;
- `[DONE]`;
- maximum event bytes.

Representative streams split at every byte boundary in tests.

Transport registry keyed by `ModelAttemptId`.

Shutdown:

1. stop accepting;
2. cancel active attempts;
3. await terminal outcomes;
4. close pool connections;
5. join request tasks;
6. return shutdown result.

Backpressure uses bounded channels and pauses network reads when runtime is slow.

Typed failure categories:

```rust
enum ModelFailureKind {
    Authentication,
    RateLimited,
    ContextLimit,
    ReconnectRequired,
    Protocol,
    Transport,
    Timeout,
    ProviderRejected,
}
```

No substring-based behavior routing.

## Provider design

Transactional switch:

```text
Requested
  -> ResolvingCredentials
  -> LoadingCatalog
  -> ValidatingModel
  -> StartingTransport
  -> PersistingSelection
  -> Ready
```

Only after all succeed does active provider generation swap. Old provider remains active on failure. Old transport shutdown is joined after swap.

Every provider and request carries `ProviderGeneration`.

Routes are explicit. Missing secondary route returns `RouteUnavailable`, not silent fallback.

Capabilities are provider-neutral and tool selection is capability-based, not provider-name based.

Telemetry receives observed:

- timestamps;
- TTFT;
- outcome;
- model usage when available;
- tool counts.

Unavailable fields remain absent.

Telemetry worker has bounded queue, supervised task, bounded shutdown flush, and isolated failure.

## Tool execution

`WorkspaceRoot` constructed from opened root directory.

All file operations use validated relative paths and directory-relative Linux operations.

Resolution policy:

- beneath root;
- no magic links;
- explicit symlink policy;
- no absolute paths;
- no parent traversal;
- no re-resolution outside capability.

Patch transaction:

1. plan against opened identities;
2. record metadata/hash;
3. stage replacements;
4. fsync staged files;
5. write/fsync intent;
6. revalidate source identities;
7. apply operations;
8. fsync affected directories;
9. write completion marker;
10. remove journal.

Promise recoverable transaction, not multi-file atomicity.

PTY state:

```text
Starting
  -> Running
  -> Exited | Failed
  -> Closing
  -> Closed
```

Each PTY owns:

- child;
- writer;
- reader;
- bounded output ring;
- cancellation token;
- join handles.

No global mutex during waits.

Output overflow uses explicit omission boundary and bounded recent output.

Every tool execution has ID, cancellation token, deadline policy, and workspace capability.

Subagents use real message mailboxes:

```rust
AgentMessage {
    message_id,
    text,
    delivery_mode,
}
```

Queue/interrupt text cannot be discarded. Child runtimes use hierarchical cancellation and joined shutdown.

## Persist mode

Replace booleans/prose scan with:

```rust
enum PersistState {
    Disabled,
    Active(PersistTask),
    Paused(PersistTask),
    Completed(PersistTask),
}

struct PersistTask {
    instruction: String,
    completion_policy: CompletionPolicy,
}

enum CompletionPolicy {
    ModelMayComplete,
    UserOnly,
}
```

Completion policy comes from explicit command/option.

`mark_task_complete` is conversation-runtime control, not filesystem tool.

## Compaction

Pure planner consumes:

- canonical history revision;
- context limits;
- token accounting;
- trigger.

Produces exact immutable range plan.

Supervised job records:

- base revision;
- source range;
- route;
- attempt;
- retry policy.

Result commits only if base revision remains current.

Checkpoint persistence succeeds before canonical history replacement.


## Implementation phases

### Phase 0: Characterization and decisions

1. capture black-box required behavior;
2. build representative session corpus;
3. classify six current core test failures;
4. repair/retire stale core benchmark;
5. decide old-session import or clean break;
6. inventory provider/auth import;
7. document exact prompt/steering whitespace semantics.

Deliverable: behavior matrix and migration decision record.

### Phase 1: Foundational APIs

Create:

```text
harness-tool-api
harness-model-api
harness-runtime-api
```

Acceptance:

- no filesystem/network/provider implementation;
- invalid tool variants unrepresentable;
- distinct model terminal outcomes;
- sequence identity preserved;
- bounded runtime channels;
- no old-core dependency;
- focused contract tests;
- no mirror tests.

### Phase 2: Responses protocol

Create:

```text
harness-responses-protocol
```

Acceptance:

- typed encoding/decoding;
- byte-split SSE tests;
- incomplete UTF-8;
- malformed frames;
- event-size bounds;
- decode tools once;
- no networking.

### Phase 3: Responses transport

Create:

```text
harness-responses-transport
```

Acceptance:

- incremental HTTPS;
- supervised WS/HTTPS tasks;
- cancellation;
- exactly one terminal outcome;
- joined shutdown;
- bounded delivery;
- no interrupted-as-completed;
- deterministic fake-server reconnect tests.

### Phase 4: Provider

Create:

```text
harness-provider
```

Acceptance:

- profiles/credentials resolved once;
- immutable `ResolvedProvider`;
- explicit routes;
- no silent fallback;
- provider generation;
- transactional switch;
- capability-based tool policy;
- observed telemetry only.

### Phase 5: Session store boundary

This phase does not redesign or migrate the `.nhsession` format. It establishes the replacement store API, isolates the current-format adapter, and implements the store behavior that does not require an on-disk format change.

Create:

```text
harness-session-store
```

Acceptance:

- session-store API is independent of provider implementation;
- current `.nhsession` files remain readable and are not rewritten;
- current-format adapter is isolated behind the store boundary;
- unique writer lease;
- durability receipts;
- bounded blocking execution;
- entry paging;
- catalog compaction and directory fsync without format migration;
- fault injection at write boundaries;
- incomplete-turn recovery;
- no automatic format conversion or startup migration.

The target strict schema, checksummed replacement chunks, contiguous-sequence replacement format, and explicit offline importer remain final-phase work.


### Phase 6: Tool execution

Create:

```text
harness-tool-execution
```

Acceptance:

- workspace capability;
- beneath-root operations;
- symlink-race tests;
- recoverable journal;
- source revalidation;
- one task per PTY;
- bounded output;
- joined readers/children;
- cancellation;
- no global lock during waits.

### Phase 7: Conversation runtime vertical slices

Create:

```text
harness-conversation-runtime
```

Slice A:

```text
prompt -> durable input -> model stream -> durable assistant -> completion
```

Slice B:

- exact steering;
- immediate interrupt;
- interrupted outcome;
- immutable retry.

Slice C:

- durable tool call before execution;
- durable result before continuation;
- unknown-after-restart recovery.

Slice D:

- revision-bound compaction;
- supervised jobs;
- durable checkpoint.

Slice E:

- typed persist state;
- explicit completion policy;
- no prose scanning.

Slice F:

- queued provider switch while active;
- transactional idle commit;
- generation isolation.

Slice G:

- real subagent mailbox;
- child cancellation;
- joined shutdown.

Each slice passes public runtime API using fake ports before real implementations.

### Phase 8: TUI adapter cutover

Change TUI to depend on `harness-runtime-api`.

Acceptance:

- exact snapshot round trip;
- sequence identity;
- typed provider/activity state;
- no old-core import;
- unchanged production TUI behavior.

### Phase 9: CLI atomic cutover

CLI responsibilities become:

1. parse arguments;
2. resolve state paths;
3. invoke provider/session composition services;
4. start conversation runtime;
5. run TUI;
6. await joined shutdown.

Remove duplicate transport/runtime construction.

No feature flag, environment switch, or dual core path.

### Phase 10: Session service/web cutover

- switch CLI IPC;
- switch web dependency;
- verify per-request failure isolation;
- remove old-core dependency from web.

### Phase 11: Deactivate old crates

Under current policy:

- keep old crates physically on disk;
- remove from active dependencies after cutover;
- do not delete;
- do not add runtime fallback.

### Phase 12: User-run `.nhsession` format migration (last)

This phase is intentionally last and does not run automatically.

It begins only after the replacement runtime, CLI, TUI adapter, and web cutovers pass their acceptance criteria. The implementation provides an explicit offline migration command or tool, but the user runs it as a separately supervised operation.

The migration:

1. selects the input session files explicitly;
2. validates the current-format records before conversion;
3. writes the target format without changing the originals;
4. verifies target checksums, sequence continuity, and session identity;
5. reports files that require manual resolution;
6. changes runtime configuration only after the user confirms the converted corpus.

Acceptance:

- no startup migration;
- no transparent old/new format fallback;
- no original `.nhsession` file is removed by the migration;
- conversion is validated before runtime configuration changes;
- the user explicitly runs and confirms the migration;
- the target strict schema and replacement-format requirements are enforced only after migration.

## Testing strategy

State-machine tests for:

- runtime lifecycle;
- root turn;
- model attempt;
- cancellation;
- persist mode;
- provider switch;
- compaction;
- PTY lifecycle;
- subagent lifecycle.

Tests assert observable transitions/effects, not private representation.

Fault injection at:

- session create;
- encoding;
- partial write;
- fsync;
- catalog rename;
- directory fsync;
- model registration;
- stream start/interruption;
- tool call persistence;
- tool execution;
- tool result persistence;
- shutdown cancellation;
- provider swap.

Protocol tests:

- byte-split SSE;
- multiple events/chunk;
- UTF-8 split;
- comments;
- malformed JSON;
- oversized event;
- duplicate terminal event;
- tool delta/call order;
- WS close during stream.

Filesystem tests:

- parent traversal;
- absolute path;
- symlink final/intermediate;
- concurrent symlink swap;
- source replacement after plan;
- crash during each commit operation;
- journal recovery idempotence.

Runtime integration tests:

- no model request before durable input;
- no continuation before durable tool result;
- interruption not completion;
- retry immutable;
- provider generations isolated;
- shutdown joins jobs;
- frontend closure does not abandon durable shutdown.

End-to-end:

- new session;
- resume;
- paging;
- tool turn;
- interruption;
- provider switch;
- compaction;
- disconnect;
- terminal restoration.

## Performance plan

Runtime:

- 64 assistant delta batch;
- event routing with tool assembly;
- cancellation latency;
- 1,000-entry request prep;
- 10,000-record resume fold.

Session store:

- durable append;
- buffered append + flush;
- reverse page;
- checkpoint resume;
- catalog load/rebuild;
- concurrent reads with writer.

Protocol/transport:

- SSE throughput;
- one-byte chunks;
- large tool assembly;
- bounded backpressure;
- WS pool acquisition.

Tools:

- sustained PTY ring;
- concurrent PTYs;
- inspect normalization;
- patch planning/commit.

Use absolute results and documented machine context, not Criterion percentage labels alone.

## Final cutover acceptance

Ready only when:

1. CLI does not depend on old core or old Responses API.
2. TUI depends only on runtime API.
3. Web uses the session-store API without a sidecar service.
4. No old/new runtime switch exists.
5. Every model request has cancellation and joined task.
6. Every child service acknowledges shutdown.
7. Shutdown proves session flush, model join, tool join, PTY join, provider close, telemetry termination.
8. Canonical history changes only after durable acknowledgement.
9. Tool continuation only after durable result.
10. Completed/interrupted/cancelled/failed remain distinct.
11. Paging uses sequence identity and entry counts.
12. Strict session reader has no transparent fallback.
13. Provider routes explicit; no silent fallback.
14. Filesystem capability-rooted and race-resistant.
15. Patch operations recoverable.
16. Telemetry observed or absent, never fabricated.
17. Old crates remain physically present but inactive.
18. `.nhsession` format migration is the final phase and is explicitly run and confirmed by the user.
