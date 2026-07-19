//! Runtime anti-corruption boundary, terminal input routing, and event loop.

mod adapter;

use std::{cmp::Ordering, io, time::Duration};

use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures_util::StreamExt;
use harness_core::{
    UiSnapshot,
    actors::{ActorHandle, ActorReceiver, RuntimeCommand, RuntimeEvent},
};

use crate::{
    app::{AppEffect, Application, MouseCapture, UserCommand},
    domain::{DomainEvent, ExternalText},
    input::{HorizontalUnit, InputFragment, RawInput, VerticalDirection},
    terminal::TerminalSession,
    view::{PreparedFrame, prepare},
};

/// Maximum mailbox events reduced between terminal frame preparations.
const MAX_RUNTIME_EVENT_DRAIN: usize = 64;
/// Visual transcript rows moved by one mouse-wheel event.
const MOUSE_WHEEL_ROWS: isize = 3;

/// Runs the terminal UI against one harness runtime.
pub async fn run_with_runtime(
    snapshot: UiSnapshot,
    commands: ActorHandle<RuntimeCommand>,
    events: ActorReceiver<RuntimeEvent>,
) -> io::Result<UiSnapshot> {
    let initial = adapter::import_snapshot(snapshot);
    let mut application = Application::import(initial)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

    {
        let mut terminal = TerminalSession::enter()?;
        let mut terminal_events = EventStream::new();

        while !application.should_exit() {
            let now = std::time::Instant::now();
            let frame = prepare(&mut application, terminal.area()?, now);
            terminal.draw(&frame)?;
            let next_visual_change = application.next_visual_change_in(now);

            let effects = tokio::select! {
                terminal_event = terminal_events.next() => {
                    match terminal_event {
                        Some(Ok(event)) => route_terminal_event(
                            event,
                            &frame,
                            application.has_transcript_selection(),
                            application.mouse_capture(),
                        )
                        .map(|command| application.handle_user_command(command))
                        .unwrap_or_default(),
                        Some(Err(error)) => return Err(error),
                        None => break,
                    }
                }
                runtime_event = events.recv() => {
                    match runtime_event {
                        Ok(event) => reduce_runtime_event_batch(
                            &mut application,
                            event,
                            &events,
                        ),
                        Err(_) => application.runtime_disconnected(),
                    }
                    Vec::new()
                }
                _ = wait_for_visual_change(next_visual_change) => {
                    application.handle_visual_deadline(std::time::Instant::now())
                },
            };

            if !execute_effects(&mut application, &mut terminal, &commands, effects).await? {
                break;
            }
        }
    }

    Ok(adapter::export_snapshot(application.into_final_state()))
}

async fn wait_for_visual_change(delay: Option<Duration>) {
    match delay {
        Some(delay) => tokio::time::sleep(delay).await,
        None => std::future::pending().await,
    }
}

fn route_terminal_event(
    event: Event,
    frame: &PreparedFrame,
    transcript_selection: bool,
    mouse_capture: MouseCapture,
) -> Option<UserCommand> {
    match event {
        Event::Key(key) => route_key_event(key, frame, transcript_selection),
        Event::Paste(text) => Some(match InputFragment::<RawInput>::new(text).bound() {
            Ok(fragment) => UserCommand::Insert(fragment),
            Err(error) => UserCommand::InputRejected(error),
        }),
        Event::Mouse(mouse) => route_mouse_event(mouse, frame, mouse_capture),
        Event::FocusLost => Some(UserCommand::CancelMouseCapture),
        Event::FocusGained | Event::Resize(_, _) => None,
    }
}

fn route_key_event(
    key: KeyEvent,
    frame: &PreparedFrame,
    transcript_selection: bool,
) -> Option<UserCommand> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let command = route_key(key, frame, transcript_selection)?;
    if key.kind == KeyEventKind::Repeat && !is_repeatable_command(&command) {
        return None;
    }
    Some(command)
}

fn route_key(
    key: KeyEvent,
    frame: &PreparedFrame,
    transcript_selection: bool,
) -> Option<UserCommand> {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let character = match key.code {
        KeyCode::Char(character) => Some(character.to_ascii_lowercase()),
        _ => None,
    };

    if control && shift && character == Some('c') && transcript_selection {
        return Some(UserCommand::CopyTranscriptSelection);
    }
    if control && character == Some('c') {
        return Some(UserCommand::InterruptKey);
    }
    if key.code == KeyCode::Esc {
        return Some(UserCommand::Interrupt);
    }
    if control && character == Some('z') {
        return Some(UserCommand::Undo);
    }
    if control && character == Some('y') {
        return Some(UserCommand::Redo);
    }
    if control && character == Some('a') {
        return Some(UserCommand::MoveLineBoundary {
            end: false,
            selecting: shift,
        });
    }
    if control && character == Some('e') {
        return Some(UserCommand::MoveLineBoundary {
            end: true,
            selecting: shift,
        });
    }
    if control && character == Some('j') {
        return Some(insert_fragment("\n"));
    }
    if transcript_selection && !control && !alt && character == Some('y') {
        return Some(UserCommand::CopyTranscriptSelection);
    }

    let (transcript_width, transcript_height) = frame.transcript_dimensions();
    let page_rows = transcript_height.saturating_sub(1).max(1) as isize;
    match key.code {
        KeyCode::Enter if key.modifiers.is_empty() => Some(UserCommand::Submit),
        KeyCode::Enter => Some(insert_fragment("\n")),
        KeyCode::Tab => Some(insert_fragment("\t")),
        KeyCode::Backspace if control || alt => Some(UserCommand::DeleteWordBackward),
        KeyCode::Backspace => Some(UserCommand::DeleteBackward),
        KeyCode::Delete => Some(UserCommand::DeleteForward),
        KeyCode::Left => Some(UserCommand::MoveHorizontal {
            direction: Ordering::Less,
            unit: if control || alt {
                HorizontalUnit::Word
            } else {
                HorizontalUnit::Grapheme
            },
            selecting: shift,
        }),
        KeyCode::Right => Some(UserCommand::MoveHorizontal {
            direction: Ordering::Greater,
            unit: if control || alt {
                HorizontalUnit::Word
            } else {
                HorizontalUnit::Grapheme
            },
            selecting: shift,
        }),
        KeyCode::Up => Some(UserCommand::MoveVertical {
            direction: VerticalDirection::Up,
            width: frame.prompt_width(),
            selecting: shift,
        }),
        KeyCode::Down => Some(UserCommand::MoveVertical {
            direction: VerticalDirection::Down,
            width: frame.prompt_width(),
            selecting: shift,
        }),
        KeyCode::Home if control => Some(UserCommand::ScrollTranscriptTop {
            width: transcript_width,
            height: transcript_height,
        }),
        KeyCode::End if control => Some(UserCommand::ScrollTranscriptBottom),
        KeyCode::Home => Some(UserCommand::MoveLineBoundary {
            end: false,
            selecting: shift,
        }),
        KeyCode::End => Some(UserCommand::MoveLineBoundary {
            end: true,
            selecting: shift,
        }),
        KeyCode::PageUp => Some(UserCommand::ScrollTranscript {
            width: transcript_width,
            height: transcript_height,
            lines: page_rows,
        }),
        KeyCode::PageDown => Some(UserCommand::ScrollTranscript {
            width: transcript_width,
            height: transcript_height,
            lines: -page_rows,
        }),
        KeyCode::Char(character) if !control && !alt => {
            Some(insert_fragment(character.to_string()))
        }
        _ => None,
    }
}

fn insert_fragment(text: impl Into<String>) -> UserCommand {
    match InputFragment::<RawInput>::new(text).bound() {
        Ok(fragment) => UserCommand::Insert(fragment),
        Err(error) => UserCommand::InputRejected(error),
    }
}

fn is_repeatable_command(command: &UserCommand) -> bool {
    matches!(
        command,
        UserCommand::Insert(_)
            | UserCommand::DeleteBackward
            | UserCommand::DeleteForward
            | UserCommand::DeleteWordBackward
            | UserCommand::MoveHorizontal { .. }
            | UserCommand::MoveVertical { .. }
            | UserCommand::MoveLineBoundary { .. }
            | UserCommand::ScrollTranscript { .. }
    )
}

fn route_mouse_event(
    mouse: MouseEvent,
    frame: &PreparedFrame,
    capture: MouseCapture,
) -> Option<UserCommand> {
    let (width, height) = frame.transcript_dimensions();
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some((top_line, thumb_offset)) = frame.transcript_scrollbar_press(mouse) {
                Some(UserCommand::BeginTranscriptScrollbarDrag {
                    width,
                    height,
                    top_line,
                    thumb_offset,
                })
            } else if let Some(position) = frame.prompt_position(mouse) {
                Some(UserCommand::BeginPromptSelection { position })
            } else if frame.transcript_contains(mouse) {
                Some(UserCommand::BeginTranscriptSelection {
                    position: frame.transcript_position(mouse),
                })
            } else {
                None
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => match capture {
            MouseCapture::Prompt => Some(UserCommand::DragSelection {
                prompt_position: frame.prompt_position_clamped(mouse),
                transcript_position: None,
            }),
            MouseCapture::Transcript => {
                if let Some((direction, cell)) = frame.transcript_selection_scroll(mouse) {
                    Some(UserCommand::DragTranscriptSelectionEdge {
                        width,
                        height,
                        direction,
                        cell,
                    })
                } else {
                    Some(UserCommand::DragSelection {
                        prompt_position: None,
                        transcript_position: frame.transcript_position_clamped(mouse),
                    })
                }
            }
            MouseCapture::TranscriptScrollbar { thumb_offset } => frame
                .transcript_scrollbar_top_line_clamped(mouse, thumb_offset)
                .map(|top_line| UserCommand::DragTranscriptScrollbar {
                    width,
                    height,
                    top_line,
                }),
            MouseCapture::None => None,
        },
        MouseEventKind::Up(MouseButton::Left) => match capture {
            MouseCapture::Prompt => Some(UserCommand::FinishSelection {
                prompt_position: frame.prompt_position_clamped(mouse),
                transcript_position: None,
            }),
            MouseCapture::Transcript => Some(UserCommand::FinishSelection {
                prompt_position: None,
                transcript_position: frame.transcript_position_clamped(mouse),
            }),
            MouseCapture::TranscriptScrollbar { thumb_offset } => {
                Some(UserCommand::FinishTranscriptScrollbar {
                    width,
                    height,
                    top_line: frame.transcript_scrollbar_top_line_clamped(mouse, thumb_offset),
                })
            }
            MouseCapture::None => None,
        },
        MouseEventKind::ScrollUp if frame.transcript_contains(mouse) => {
            Some(UserCommand::ScrollTranscript {
                width,
                height,
                lines: MOUSE_WHEEL_ROWS,
            })
        }
        MouseEventKind::ScrollDown if frame.transcript_contains(mouse) => {
            Some(UserCommand::ScrollTranscript {
                width,
                height,
                lines: -MOUSE_WHEEL_ROWS,
            })
        }
        MouseEventKind::Down(_)
        | MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::Moved
        | MouseEventKind::ScrollUp
        | MouseEventKind::ScrollDown
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => None,
    }
}

fn reduce_runtime_event_batch(
    application: &mut Application,
    first: RuntimeEvent,
    events: &ActorReceiver<RuntimeEvent>,
) {
    let mut pending_delta = None;
    reduce_runtime_event(application, first, &mut pending_delta);
    for _ in 0..MAX_RUNTIME_EVENT_DRAIN {
        let Ok(event) = events.try_recv() else {
            break;
        };
        reduce_runtime_event(application, event, &mut pending_delta);
        if application.should_exit() {
            break;
        }
    }
    flush_assistant_delta(application, &mut pending_delta);
}

fn reduce_runtime_event(
    application: &mut Application,
    event: RuntimeEvent,
    pending_delta: &mut Option<String>,
) {
    match event {
        RuntimeEvent::AssistantTextDelta(delta) => {
            pending_delta
                .get_or_insert_with(String::new)
                .push_str(&delta);
        }
        event => {
            flush_assistant_delta(application, pending_delta);
            application.apply_domain_event(adapter::adapt_runtime_event(event));
        }
    }
}

fn flush_assistant_delta(application: &mut Application, pending_delta: &mut Option<String>) {
    if let Some(delta) = pending_delta.take() {
        application.apply_domain_event(DomainEvent::AssistantTextDelta(ExternalText::new(delta)));
    }
}

async fn execute_effects(
    application: &mut Application,
    terminal: &mut TerminalSession,
    commands: &ActorHandle<RuntimeCommand>,
    effects: Vec<AppEffect>,
) -> io::Result<bool> {
    for effect in effects {
        match effect {
            AppEffect::Runtime {
                request,
                completion,
            } => {
                match commands
                    .send(adapter::export_runtime_request(request))
                    .await
                {
                    Ok(()) => application.delivery_accepted(completion),
                    Err(error) => {
                        application.delivery_failed(
                            completion,
                            format!("runtime command delivery failed: {error}"),
                        );
                        application.runtime_disconnected();
                        return Ok(false);
                    }
                }
            }
            AppEffect::Clipboard(text) => terminal.copy_to_clipboard(&text)?,
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_control_text_is_preserved_for_editor_insertion() {
        let source = "\u{1b}[31m\r\n";
        let Some(UserCommand::Insert(fragment)) = route_terminal_event(
            Event::Paste(source.to_string()),
            &prepared_frame(),
            false,
            MouseCapture::None,
        ) else {
            panic!("bounded paste routes to editor insertion");
        };

        assert_eq!(fragment.as_str(), source);
    }

    fn prepared_frame() -> PreparedFrame {
        let mut application = Application::import(crate::domain::InitialState {
            session_id: crate::domain::ExternalText::new("session"),
            thread_title: crate::domain::ExternalText::new("thread"),
            provider: None,
            model: crate::domain::ModelState {
                model: crate::domain::ExternalText::new("model"),
                reasoning_effort: None,
                service_tier: None,
            },
            developer_mode: false,
            response_streaming: false,
            last_ttft_ms: None,
            transcript: Vec::new(),
            prompt: String::new(),
            prompt_cursor: 0,
            queued_steering: None,
            agents: Vec::new(),
            active_activity_ids: Vec::new(),
        })
        .unwrap();
        prepare(
            &mut application,
            ratatui::layout::Rect::new(0, 0, 80, 24),
            std::time::Instant::now(),
        )
    }
    fn application_with_transcript() -> Application {
        let transcript = (0..40)
            .map(|index| crate::domain::TranscriptSnapshotEntry {
                sequence: None,
                payload: crate::domain::TranscriptPayload::Message {
                    role: crate::domain::MessageRole::Assistant,
                    text: crate::domain::ExternalText::new(format!("entry {index}")),
                },
            })
            .collect();
        Application::import(crate::domain::InitialState {
            session_id: crate::domain::ExternalText::new("session"),
            thread_title: crate::domain::ExternalText::new("thread"),
            provider: None,
            model: crate::domain::ModelState {
                model: crate::domain::ExternalText::new("model"),
                reasoning_effort: None,
                service_tier: None,
            },
            developer_mode: false,
            response_streaming: false,
            last_ttft_ms: None,
            transcript,
            prompt: String::new(),
            prompt_cursor: 0,
            queued_steering: None,
            agents: Vec::new(),
            active_activity_ids: Vec::new(),
        })
        .unwrap()
    }

    #[test]
    fn scrollbar_drag_reaches_both_transcript_extents_without_press_jump() {
        let area = ratatui::layout::Rect::new(0, 0, 30, 12);
        let mut application = application_with_transcript();
        let frame = prepare(&mut application, area, std::time::Instant::now());
        let transcript_area = frame.areas().transcript;
        let scrollbar_column = transcript_area
            .x
            .saturating_add(transcript_area.width.saturating_sub(1));
        let bottom_row = transcript_area
            .y
            .saturating_add(transcript_area.height.saturating_sub(1));
        let (width, height) = frame.transcript_dimensions();
        let current_top = application
            .transcript_mut()
            .viewport(width, height)
            .top_line;

        let press = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: scrollbar_column,
            row: bottom_row,
            modifiers: KeyModifiers::NONE,
        };
        let command =
            route_mouse_event(press, &frame, MouseCapture::None).expect("scrollbar press routes");
        application.handle_user_command(command);
        assert_eq!(
            application
                .transcript_mut()
                .viewport(width, height)
                .top_line,
            current_top,
        );

        let top_drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: scrollbar_column,
            row: transcript_area.y,
            modifiers: KeyModifiers::NONE,
        };
        let command = route_mouse_event(top_drag, &frame, application.mouse_capture())
            .expect("top scrollbar drag routes");
        application.handle_user_command(command);
        assert_eq!(
            application
                .transcript_mut()
                .viewport(width, height)
                .top_line,
            0,
        );

        let frame = prepare(&mut application, area, std::time::Instant::now());
        let bottom_drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: scrollbar_column,
            row: bottom_row,
            modifiers: KeyModifiers::NONE,
        };
        let command = route_mouse_event(bottom_drag, &frame, application.mouse_capture())
            .expect("bottom scrollbar drag routes");
        application.handle_user_command(command);
        let viewport = application.transcript_mut().viewport(width, height);
        assert_eq!(
            viewport.top_line,
            viewport.total_lines.saturating_sub(viewport.height),
        );

        let frame = prepare(&mut application, area, std::time::Instant::now());
        let release = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: scrollbar_column,
            row: bottom_row,
            modifiers: KeyModifiers::NONE,
        };
        let command = route_mouse_event(release, &frame, application.mouse_capture())
            .expect("scrollbar release routes");
        application.handle_user_command(command);
        assert_eq!(application.mouse_capture(), MouseCapture::None);
    }
}
