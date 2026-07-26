use std::time::Instant;

use ratatui::{Frame, layout::Rect};

use crate::{
    app::Application,
    domain::InitialState,
    view::{prepare, render},
};

pub fn render_preview(initial: InitialState, frame: &mut Frame<'_>, area: Rect) {
    if let Ok(mut app) = Application::import(initial) {
        let prepared = prepare(&mut app, area, Instant::now());
        render(frame, &prepared);
    }
}
