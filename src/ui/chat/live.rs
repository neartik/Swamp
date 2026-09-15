use crossterm::queue;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::io::{self, Stdout, Write};

/// The live tail of the chat: the last `height` rows of the real screen, never an alternate
/// one. Everything above it is the terminal's own scrollback, selectable with the mouse.
///
/// `Viewport::Fixed` rather than `Viewport::Inline`, and the scroll is done by hand: an
/// inline viewport re-reads the cursor position on every resize, and that read is answered
/// on stdin, which the key `EventStream` is already draining. The two cannot coexist.
///
/// Two rules keep committed rows safe:
///
/// - the area only grows by printing newlines on the *last* row, so the host scrolls committed
///   rows up into real scrollback. Nothing above the viewport is ever cleared.
/// - the area never shrinks on its own. The surplus rows stay inside the viewport, blank, and
///   [`Inline::commit`] fills them with finished lines before it scrolls, so no blank row is
///   ever pushed into scrollback between two committed blocks.
///
/// Every byte goes through the backend's writer, which is what lets the tests point one at a
/// terminal emulator.
pub struct Inline<W: Write> {
    term: Terminal<CrosstermBackend<W>>,
    /// Rows the viewport owns on screen.
    height: u16,
    /// Rows the live content needs; `height - want` blank rows pad the top.
    want: u16,
    width: u16,
    rows: u16,
}

impl<W: Write> Inline<W> {
    /// `cursor_row` is where the host's own output stopped: the area scrolls only as far as it
    /// takes to free the bottom `height` rows.
    pub fn new(
        out: W,
        width: u16,
        rows: u16,
        height: u16,
        cursor_row: u16,
    ) -> io::Result<Inline<W>> {
        let height = height.clamp(1, rows);
        let mut term = Terminal::with_options(
            CrosstermBackend::new(out),
            TerminalOptions {
                viewport: Viewport::Fixed(area(width, rows, height)),
            },
        )?;
        let free = rows.saturating_sub(1).saturating_sub(cursor_row);
        scroll(&mut term, rows, height.saturating_sub(free))?;
        Ok(Inline {
            term,
            height,
            want: height,
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
        self.height = self.height.min(rows);
        self.want = self.want.min(self.height);
        self.term.resize(area(width, rows, self.height))
    }

    /// Growing means scrolling the host terminal first: the new rows have to exist before
    /// anything is drawn into them. Shrinking is deferred to `commit`.
    pub fn set_height(&mut self, want: u16) -> io::Result<()> {
        let want = want.clamp(1, self.rows);
        self.want = want;
        if want <= self.height {
            return Ok(());
        }
        scroll(&mut self.term, self.rows, want - self.height)?;
        self.height = want;
        self.term.resize(area(self.width, self.rows, self.height))
    }

    pub fn draw(&mut self, lines: Vec<Line<'static>>, cursor: (u16, u16)) -> io::Result<()> {
        let pad = self.height.saturating_sub(self.want);
        let (row, col) = cursor;
        self.term.draw(|f| {
            let area = f.area();
            let mut padded = vec![Line::default(); pad as usize];
            padded.extend(lines);
            f.render_widget(Paragraph::new(padded), area);
            let row = row + pad;
            if row < area.height {
                f.set_cursor_position(Position::new(
                    area.x + col.min(area.width.saturating_sub(1)),
                    area.y + row,
                ));
            }
        })?;
        Ok(())
    }

    /// Pushes finished blocks into scrollback. Surplus rows left by a shrunken live area are
    /// filled first; past that each line is drawn on the viewport's top row and then scrolled
    /// off it, which is what puts it above the live area for good.
    ///
    /// Lines arrive pre-wrapped; nothing here wraps, so nothing here can clip.
    pub fn commit(&mut self, lines: Vec<Line<'static>>) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let width = self.width;
        let mut fill = self.height - self.want;
        let mut y = self.rows - self.height;
        for line in lines {
            write_row(&mut self.term, line, y, width)?;
            if fill > 0 {
                fill -= 1;
                self.height -= 1;
                y += 1;
            } else {
                scroll(&mut self.term, self.rows, 1)?;
            }
        }
        self.term.resize(area(self.width, self.rows, self.height))
    }

    pub fn clear_screen(&mut self) -> io::Result<()> {
        use crossterm::cursor::MoveTo;
        use crossterm::terminal::{Clear, ClearType};
        self.height = self.want;
        queue!(
            self.term.backend_mut(),
            Clear(ClearType::All),
            MoveTo(0, self.rows.saturating_sub(self.height))
        )?;
        Backend::flush(self.term.backend_mut())?;
        self.term.resize(area(self.width, self.rows, self.height))
    }
}

/// Raw mode, the kitty flags `shift+enter` needs, and one cursor-position read. The read is
/// answered on stdin, so it has to happen before the key `EventStream` starts draining it.
pub fn enter(width: u16, rows: u16, height: u16) -> io::Result<Inline<Stdout>> {
    use crossterm::cursor::SetCursorStyle;
    use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
    use crossterm::execute;
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
    let cursor_row = crossterm::cursor::position()
        .map(|(_, y)| y)
        .unwrap_or(rows.saturating_sub(1));
    Inline::new(io::stdout(), width, rows, height, cursor_row.min(rows.saturating_sub(1)))
}

fn area(width: u16, rows: u16, height: u16) -> Rect {
    Rect::new(0, rows.saturating_sub(height), width, height)
}

/// `n` newlines on the last row: the only way to make the host scroll committed rows into its
/// own scrollback.
fn scroll<W: Write>(term: &mut Terminal<CrosstermBackend<W>>, rows: u16, n: u16) -> io::Result<()> {
    if n == 0 {
        return Ok(());
    }
    use crossterm::cursor::MoveTo;
    use crossterm::style::Print;
    queue!(
        term.backend_mut(),
        MoveTo(0, rows.saturating_sub(1)),
        Print("\n".repeat(n as usize))
    )?;
    Backend::flush(term.backend_mut())
}

fn write_row<W: Write>(
    term: &mut Terminal<CrosstermBackend<W>>,
    line: Line<'static>,
    y: u16,
    width: u16,
) -> io::Result<()> {
    let rect = Rect::new(0, y, width, 1);
    let mut buffer = Buffer::empty(rect);
    Paragraph::new(line).render(rect, &mut buffer);
    let cells = buffer
        .content
        .iter()
        .enumerate()
        .map(|(i, cell)| (i as u16, y, cell));
    term.backend_mut().draw(cells)?;
    Backend::flush(term.backend_mut())
}

/// Never leaves an alternate screen: chat was never on one.
pub fn restore_inline() -> io::Result<()> {
    use crossterm::cursor::Show;
    use crossterm::event::PopKeyboardEnhancementFlags;
    use crossterm::execute;
    use crossterm::terminal::disable_raw_mode;
    disable_raw_mode()?;
    let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags, Show);
    Ok(())
}

#[cfg(test)]
mod tests;
