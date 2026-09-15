use crossterm::execute;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::io::{self, Stdout};

/// The live tail of the chat: the last `height` rows of the real screen, never an alternate
/// one. Everything above it is the terminal's own scrollback, selectable with the mouse.
///
/// `Viewport::Fixed` rather than `Viewport::Inline`, and the scroll is done by hand: an
/// inline viewport re-reads the cursor position on every resize, and that read is answered
/// on stdin, which the key `EventStream` is already draining. The two cannot coexist.
pub struct Inline {
    term: Terminal<CrosstermBackend<Stdout>>,
    height: u16,
    width: u16,
    rows: u16,
}

impl Inline {
    pub fn enter(width: u16, rows: u16, height: u16) -> io::Result<Inline> {
        use crossterm::cursor::SetCursorStyle;
        use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
        use crossterm::style::Print;
        use crossterm::terminal::enable_raw_mode;
        enable_raw_mode()?;
        // shift+enter only reaches crossterm under the kitty protocol.
        let _ = execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            )
        );
        let _ = execute!(io::stdout(), SetCursorStyle::SteadyBlock);
        // Scroll the host terminal until the rows this viewport owns are the last ones.
        execute!(io::stdout(), Print("\n".repeat(height as usize)))?;
        let term = Terminal::with_options(
            CrosstermBackend::new(io::stdout()),
            TerminalOptions {
                viewport: Viewport::Fixed(area(width, rows, height)),
            },
        )?;
        Ok(Inline {
            term,
            height,
            width,
            rows,
        })
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn set_size(&mut self, width: u16, rows: u16) -> io::Result<()> {
        if (width, rows) == (self.width, self.rows) {
            return Ok(());
        }
        self.width = width;
        self.rows = rows;
        let height = self.height.min(rows);
        self.height = height;
        self.term.resize(area(width, rows, height))
    }

    /// Growing means scrolling the host terminal first: the new rows have to exist before
    /// anything is drawn into them.
    pub fn set_height(&mut self, want: u16) -> io::Result<()> {
        let want = want.clamp(1, self.rows);
        if want == self.height {
            return Ok(());
        }
        use crossterm::cursor::MoveTo;
        use crossterm::style::Print;
        use crossterm::terminal::{Clear, ClearType};
        let top = self.rows.saturating_sub(self.height.max(want));
        execute!(io::stdout(), MoveTo(0, top), Clear(ClearType::FromCursorDown))?;
        if want > self.height {
            execute!(io::stdout(), Print("\n".repeat((want - self.height) as usize)))?;
        }
        self.height = want;
        self.term.resize(area(self.width, self.rows, want))
    }

    pub fn draw(&mut self, lines: Vec<Line<'static>>, cursor: (u16, u16)) -> io::Result<()> {
        let (row, col) = cursor;
        self.term.draw(|f| {
            let area = f.area();
            f.render_widget(Paragraph::new(lines), area);
            if row < area.height {
                f.set_cursor_position(Position::new(
                    area.x + col.min(area.width.saturating_sub(1)),
                    area.y + row,
                ));
            }
        })?;
        Ok(())
    }

    /// Pushes finished blocks into scrollback: each line is drawn on the viewport's top row
    /// and then scrolled off it, which is what puts it above the live area for good.
    ///
    /// Lines arrive pre-wrapped; nothing here wraps, so nothing here can clip.
    pub fn commit(&mut self, lines: Vec<Line<'static>>) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        use crossterm::cursor::MoveTo;
        use crossterm::style::Print;
        let top = self.rows.saturating_sub(self.height);
        let width = self.width;
        for line in lines {
            let rect = Rect::new(0, top, width, 1);
            let mut buffer = Buffer::empty(rect);
            Paragraph::new(line).render(rect, &mut buffer);
            let cells = buffer
                .content
                .iter()
                .enumerate()
                .map(|(i, cell)| (i as u16, top, cell));
            self.term.backend_mut().draw(cells)?;
            // One newline on the last row scrolls the whole screen up by one, which carries
            // the row just drawn out of the live area.
            execute!(io::stdout(), MoveTo(0, self.rows - 1), Print("\n"))?;
        }
        self.term.clear()
    }

    pub fn clear_screen(&mut self) -> io::Result<()> {
        use crossterm::cursor::MoveTo;
        use crossterm::terminal::{Clear, ClearType};
        execute!(io::stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
        execute!(
            io::stdout(),
            crossterm::style::Print("\n".repeat(self.height as usize))
        )?;
        self.term.clear()
    }
}

fn area(width: u16, rows: u16, height: u16) -> Rect {
    Rect::new(0, rows.saturating_sub(height), width, height)
}

/// Never leaves an alternate screen: chat was never on one.
pub fn restore_inline() -> io::Result<()> {
    use crossterm::cursor::Show;
    use crossterm::event::PopKeyboardEnhancementFlags;
    use crossterm::terminal::disable_raw_mode;
    disable_raw_mode()?;
    let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags, Show);
    Ok(())
}
