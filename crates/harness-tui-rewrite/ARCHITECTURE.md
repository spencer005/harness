# Harness TUI architecture

## Purpose

This crate owns terminal interaction and presentation for an interactive harness
session. It treats every value received from the runtime, terminal, or persisted
snapshot as untrusted input.

The crate does not depend on the previous `harness-tui` implementation. Runtime
integration is restricted to the `runtime` module; application, input,
transcript, and view modules use rewrite-owned domain types.

## Dependency direction

```text
terminal events ─┐
                 ├─> runtime adapter / input router ─> application reducer
runtime events ──┘                                      │
                                                        v
terminal backend <─ laid-out safe frame <─ view preparation
```

Dependencies point toward domain state. The terminal backend does not mutate
application state, and the application does not construct Ratatui values.

## Display trust boundary

Runtime and tool text follows a consuming typestate pipeline:

```text
DisplayDocument<Raw>
    -> DisplayDocument<Parsed>
    -> DisplayDocument<ControlFree>
    -> DisplayDocument<Bounded>
    -> DisplayDocument<LaidOut>
```

Only `DisplayDocument<LaidOut>` exposes conversion to Ratatui lines. Stage
constructors and document storage remain private to the display module.

The parser recognizes terminal escape families and converts supported SGR
sequences into typed styles. Sanitization removes all remaining control and
bidirectional formatting characters. Newlines are structural line boundaries,
not characters in display runs.

Prompt text has different ownership semantics. Keyboard and paste input is
stored and submitted exactly, subject only to explicit resource limits and
UTF-8/grapheme cursor invariants. A separate safe visual projection represents
control characters with visible glyphs before prompt text enters the display
pipeline. Thus pasted terminal sequences remain part of the user's submitted
text but can never execute while the prompt is rendered.

Compact status and notice labels use a dedicated one-row transition. The
transition reserves a one-cell omission marker before retaining source
graphemes, so terminal width zero produces no cells and width one produces only
the marker when content does not fit. Rendering never relies on backend
wrapping or clipping to define the first visible row.
## Mutable ownership

`Application` composes independent state owners:

- `PromptEditor` owns exact prompt text, cursor, selection, operation-based edit
  history, and pending submission transitions.
- `Transcript` owns complete transcript identity, paging state, streaming state,
  viewport position, selection, and reconstructible projection caches.
- `SessionState` owns provider, model, activity, agent, and runtime status.
- `InteractionState` owns focus, mutually exclusive mouse capture, transcript
  edge-scroll deadlines, exit confirmation, and notices.

`UiSnapshot` is imported at startup and exported at shutdown. It is not mutable
application storage.

## Coordinates

Text storage uses UTF-8 byte positions that are constructible only at grapheme
boundaries. Display layout uses terminal cell widths. Prompt cursor placement,
mouse hit testing, wrapping, and vertical movement share one `PromptLayout`.


`PromptEditor` caches a reconstructible sparse row index by exact text revision
and content width. The index retains periodic source-byte checkpoints rather
than copied display text or one cursor stop per grapheme. Frame preparation
materializes only the rows visible around the cursor, and rendering, mouse hit
testing, cursor placement, and vertical movement all resolve through that same
index. Visible rows have independent total byte and hit-map budgets, and a
pathologically large single grapheme has its own projection bound. Exhausted
presentation budgets use visible omission glyphs while storage, editing,
cursor identity, and submission retain exact source bytes.
Transcript selection positions pair stable entry IDs with selectable-text byte
offsets. Viewport anchors are a distinct type: entry rows use byte offsets in
the complete control-free projection, including nonselectable decorations, and
synthetic separators use the preceding stable entry ID. Exact row starts take
priority over half-open containment when wrapping changes. Wrapped visual-line
numbers remain derived frame data and are never stored as transcript identity.

Mouse gestures use explicit capture ownership. Prompt selection, transcript
selection, and scrollbar dragging cannot handle the same drag event. Transcript
selection scrolls one wrapped row at a time while the captured pointer remains
beyond a viewport edge, using a short deadline only for the active gesture.
Scrollbar rendering and hit testing share one proportional-thumb geometry;
capture retains the pointer's offset inside the thumb so pressing it does not
jump the viewport.

## Effects

Input reduction creates typed effects. Runtime command delivery is asynchronous
and waits for bounded mailbox capacity. Prompt submission clears the editor only
after command acceptance; a closed mailbox leaves the exact draft unchanged.

Clipboard output accepts only `ClipboardText`, which is constructible from
control-free selected text. OSC-52 delimiters are static, and the payload is
base64 encoded under an explicit byte limit.

## Transcript storage

`Transcript` retains every imported, paged, and live semantic entry. Canonical
transcript state is not evicted by UI entry or payload-byte thresholds. Resource
limits apply to individual untrusted display documents, prompt storage,
clipboard payloads, and reconstructible layout caches; they do not silently
discard transcript truth.

Historical paging is a runtime/storage concern expressed in persisted entry
identity. Page entry counts are not terminal rows and never influence wrapping,
viewport coordinates, or retention. A request retains its sequence cursor until
the matching response arrives. A nonterminal response must move to an older
cursor and contain a previously unseen entry; a response that violates either
condition enters a non-requestable protocol-error state.

## Testing

Tests target parsing invariants, state transitions, Unicode cell layout,
identity-preserving transcript operations, and effect commit/rollback. Tests do
not assert private container representation merely because the implementation
currently uses it.

The `performance` Criterion target measures prompt indexing, visible prompt
projection, Unicode movement, replacement edits, transcript layout and reflow,
stream batches, page prepend, selection extraction, frame preparation, and
frame rendering. The target compiles the private rewrite modules directly and
does not widen the production library API.
