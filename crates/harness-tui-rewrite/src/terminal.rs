//! Terminal capability acquisition, restoration, and clipboard output.

use std::{
    io::{self, Stdout, Write},
    time::Instant,
};

use crossterm::{
    cursor::MoveTo,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Terminal,
    backend::{Backend, CrosstermBackend},
};

use crate::{
    app::Application,
    display::ClipboardText,
    view::{PreparedFrame, prepare, render},
};

/// Terminal capabilities acquired by the session in acquisition order.
#[derive(Debug, Default)]
struct AcquiredCapabilities {
    raw_mode: bool,
    alternate_screen: bool,
    bracketed_paste: bool,
    mouse_capture: bool,
    keyboard_enhancements: bool,
}

impl AcquiredCapabilities {
    fn restore(&mut self, writer: &mut impl Write) {
        if self.keyboard_enhancements {
            let _ = execute!(writer, PopKeyboardEnhancementFlags);
            self.keyboard_enhancements = false;
        }
        if self.mouse_capture {
            let _ = execute!(writer, DisableMouseCapture);
            self.mouse_capture = false;
        }
        if self.bracketed_paste {
            let _ = execute!(writer, DisableBracketedPaste);
            self.bracketed_paste = false;
        }
        if self.alternate_screen {
            let _ = execute!(writer, LeaveAlternateScreen);
            self.alternate_screen = false;
        }
        if self.raw_mode {
            let _ = terminal::disable_raw_mode();
            self.raw_mode = false;
        }
    }
}

/// Partially acquired terminal state that restores itself on setup failure.
struct Acquisition {
    stdout: Stdout,
    capabilities: AcquiredCapabilities,
}

impl Acquisition {
    fn begin() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self {
            stdout: io::stdout(),
            capabilities: AcquiredCapabilities {
                raw_mode: true,
                ..AcquiredCapabilities::default()
            },
        })
    }

    fn acquire(mut self) -> io::Result<Self> {
        execute!(self.stdout, EnterAlternateScreen)?;
        self.capabilities.alternate_screen = true;

        execute!(self.stdout, Clear(ClearType::All), MoveTo(0, 0))?;

        execute!(self.stdout, EnableBracketedPaste)?;
        self.capabilities.bracketed_paste = true;

        execute!(self.stdout, EnableMouseCapture)?;
        self.capabilities.mouse_capture = true;

        execute!(
            self.stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        )?;
        self.capabilities.keyboard_enhancements = true;
        Ok(self)
    }

    fn finish(mut self) -> io::Result<TerminalSession> {
        let stdout = std::mem::replace(&mut self.stdout, io::stdout());
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        let capabilities = std::mem::take(&mut self.capabilities);
        Ok(TerminalSession {
            terminal,
            capabilities,
        })
    }
}

impl Drop for Acquisition {
    fn drop(&mut self) {
        self.capabilities.restore(&mut self.stdout);
    }
}

/// Exclusive terminal session owned by the runtime loop.
pub(crate) struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    capabilities: AcquiredCapabilities,
}

impl TerminalSession {
    /// Acquires raw mode and all terminal presentation capabilities.
    pub(crate) fn enter() -> io::Result<Self> {
        Acquisition::begin()?.acquire()?.finish()
    }

    /// Prepares and renders one frame against Ratatui's current post-resize area.
    pub(crate) fn draw(
        &mut self,
        application: &mut Application,
        now: Instant,
    ) -> io::Result<PreparedFrame> {
        draw_application_frame(&mut self.terminal, application, now)
    }

    /// Clears the physical terminal screen and invalidates Ratatui's frame buffer.
    pub(crate) fn clear(&mut self) -> io::Result<()> {
        self.terminal.clear()
    }

    /// Writes a validated clipboard payload using OSC-52.
    pub(crate) fn copy_to_clipboard(&mut self, text: &ClipboardText) -> io::Result<()> {
        write_osc52(self.terminal.backend_mut(), text)
    }
}

fn draw_application_frame<B: Backend>(
    terminal: &mut Terminal<B>,
    application: &mut Application,
    now: Instant,
) -> io::Result<PreparedFrame> {
    let mut prepared = None;
    terminal.draw(|frame| {
        let next = prepare(application, frame.area(), now);
        render(frame, &next);
        prepared = Some(next);
    })?;
    Ok(prepared.expect("successful terminal draw invokes the render callback"))
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        self.capabilities.restore(self.terminal.backend_mut());
    }
}

/// OSC-52 prefix selecting the terminal clipboard (`c`) for Base64 data.
const OSC52_CLIPBOARD_PREFIX: &[u8] = b"\x1b]52;c;";
/// Bell terminates the OSC-52 command without exposing payload bytes as control.
const OSC52_TERMINATOR: &[u8] = b"\x07";

fn write_osc52(writer: &mut impl Write, text: &ClipboardText) -> io::Result<()> {
    let encoded = base64_encode(text.as_str().as_bytes());
    writer.write_all(OSC52_CLIPBOARD_PREFIX)?;
    writer.write_all(encoded.as_bytes())?;
    writer.write_all(OSC52_TERMINATOR)?;
    writer.flush()
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        let value = (u32::from(first) << 16) | (u32::from(second) << 8) | u32::from(third);
        encoded.push(ALPHABET[((value >> 18) & 0x3f) as usize] as char);
        encoded.push(ALPHABET[((value >> 12) & 0x3f) as usize] as char);
        if chunk.len() >= 2 {
            encoded.push(ALPHABET[((value >> 6) & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() == 3 {
            encoded.push(ALPHABET[(value & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, layout::Rect};

    use crate::domain::{ExternalText, InitialState, ModelState};

    #[test]
    fn draw_prepares_render_and_interaction_geometry_after_autoresize() {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let mut application = Application::import(InitialState {
            session_id: ExternalText::new("session"),
            thread_title: ExternalText::new("thread"),
            provider: None,
            model: ModelState {
                model: ExternalText::new("model"),
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

        terminal.backend_mut().resize(12, 6);
        let prepared =
            draw_application_frame(&mut terminal, &mut application, Instant::now()).unwrap();

        assert_eq!(terminal.backend().buffer().area, Rect::new(0, 0, 12, 6));
        assert_eq!(prepared.prompt_width(), 10);
        for area in [
            prepared.areas().transcript,
            prepared.areas().transcript_content,
            prepared.areas().activity,
            prepared.areas().prompt,
            prepared.areas().prompt_content,
            prepared.areas().status,
        ] {
            assert!(area.right() <= 12);
            assert!(area.bottom() <= 6);
        }
    }


    #[test]
    fn base64_encoding_covers_partial_groups() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"h"), "aA==");
        assert_eq!(base64_encode(b"he"), "aGU=");
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn osc52_writer_uses_only_static_controls_and_encoded_payload() {
        let text = ClipboardText::from_control_free("hello".to_string()).unwrap();
        let mut output = Vec::new();
        write_osc52(&mut output, &text).unwrap();
        assert_eq!(output, b"\x1b]52;c;aGVsbG8=\x07");
    }
}
