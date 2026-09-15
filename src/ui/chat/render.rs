use crate::ui::chat::app::{App, MIN_LIVE};
use crate::ui::chat::slash;
use crate::ui::chat::theme::{Glyph, Role};
use crate::ui::fmt;
use ratatui::text::{Line, Span};

/// The live tail: everything above it is already in the terminal's own scrollback.
pub struct Live {
    pub lines: Vec<Line<'static>>,
    /// Row and column of the real terminal cursor, relative to the live area.
    pub cursor: (u16, u16),
}

pub fn live(app: &App) -> Vec<Line<'static>> {
    compose(app).lines
}

pub fn compose(app: &App) -> Live {
    let t = &app.theme;
    let width = app.width;
    let mut lines: Vec<Line<'static>> = Vec::new();
    for block in &app.blocks {
        lines.extend(block.render(&app.ctx(true)));
    }
    if let Some(working) = app.working_line() {
        if !lines.is_empty() {
            lines.push(Line::from(String::new()));
        }
        lines.push(working);
    }
    if app.overlay {
        lines.extend(slash::overlay(t));
    } else if let Some(sel) = app.popup {
        lines.extend(slash::popup(app.editor.text(), sel, width, t));
    }
    let rule = Line::from(t.span(
        t.g(Glyph::Rule).repeat(width as usize),
        Role::Meta,
    ));
    lines.push(rule.clone());

    let first_input = lines.len() as u16;
    let body = app.editor.lines();
    for (i, text) in body.iter().enumerate() {
        let lead = if i == 0 { "> " } else { "  " };
        let mut spans = vec![t.span(lead, Role::Meta), Span::raw((*text).to_owned())];
        if i == 0 && app.editor.is_empty() && app.turns == 0 && !app.working() {
            spans.push(t.span(
                format!(
                    " {}",
                    fmt::truncate(&app.placeholder, width.saturating_sub(4) as usize)
                ),
                Role::Meta,
            ));
        }
        lines.push(Line::from(spans));
    }
    lines.push(rule);
    lines.push(app.status_line());

    let (row, col) = app.editor.cursor_rc();
    Live {
        lines,
        cursor: (first_input + row as u16, 2 + col as u16),
    }
}

/// `want` rows, clamped: the conversation never owns more than three fifths of the screen.
pub fn live_height(app: &App, rows: u16) -> u16 {
    let want = live(app).len() as u16;
    let ceiling = (rows * 3 / 5).max(MIN_LIVE);
    want.clamp(MIN_LIVE, ceiling)
}
