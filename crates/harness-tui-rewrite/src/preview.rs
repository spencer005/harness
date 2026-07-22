use crate::app::Application;
use crate::domain::InitialState;
use crate::view::{prepare, render};
use ratatui::{layout::Rect, Frame};
use std::time::Instant;

pub fn render_preview(initial: InitialState, frame: &mut Frame<'_>, area: Rect) {
    if let Ok(mut app) = Application::import(initial) {
        let prepared = prepare(&mut app, area, Instant::now());
        render(frame, &prepared);
    }
}
