use crossterm::queue;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::io::{self, Stdout, Write};

/// The live tail of the chat: the rows just under the last committed block, on the real screen,
/// never an alternate one. Everything above it is the terminal's own scrollback, selectable with
/// the mouse.
///
/// `Viewport::Fixed` rather than `Viewport::Inline`, and the scroll is done by hand: an
/// inline viewport re-reads the cursor position on every resize, and that read is answered
/// on stdin, which the key `EventStream` is already draining. The two cannot coexist.
///
/// Three rules keep committed rows safe:
///
/// - the area only grows by taking rows the host screen has free below it, and past that by
///   printing newlines on the *last* row, so the host scrolls committed rows up into real
///   scrollback. Nothing above the live area is ever cleared.
/// - the area holds live content and nothing else: a shrink hands the surplus rows straight back
///   to the host, blanked, so no hole is ever parked above the input bar.
/// - those handed-back rows are remembered as [`Inline::free`], and [`Inline::commit`] writes
///   finished lines into them before it scrolls, so no blank row is ever pushed into scrollback
///   between two committed blocks.
///
/// Every byte goes through the backend's writer, which is what lets the tests point one at a
/// terminal emulator.
pub struct Inline<W: Write> {
    term: Terminal<CrosstermBackend<W>>,
    /// Rows the live content owns; it always fills them exactly.
    height: u16,
    /// Blank rows between the live area and the bottom of the screen: freed by a shrink, taken
    /// back by a grow, written into by the next commit.
    free: u16,
    width: u16,
    rows: u16,
}

impl<W: Write> Inline<W> {
    /// `cursor_row` is where the host's own output stopped: the live area opens there, and only
    /// scrolls if the screen has fewer rows left under it than the area needs.
    pub fn new(
        out: W,
        width: u16,
        rows: u16,
        height: u16,
        cursor_row: u16,
    ) -> io::Result<Inline<W>> {
        let height = height.clamp(1, rows);
        let below = rows.saturating_sub(cursor_row);
        let (top, free) = if below >= height {
            (cursor_row, below - height)
        } else {
            (rows - height, 0)
        };
        let mut term = Terminal::with_options(
            CrosstermBackend::new(out),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, top, width, height)),
            },
        )?;
        scroll(&mut term, rows, height.saturating_sub(below))?;
        Ok(Inline {
            term,
            height,
            free,
            width,
            rows,
        })
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    /// A resize invalidates the live area: the host has reflowed or truncated its rows under it.
    /// The rows from the old top down are the area's own and are blanked; anything above them is
    /// committed, so it is scrolled into scrollback rather than written over.
    pub fn set_size(&mut self, width: u16, rows: u16) -> io::Result<()> {
        if (width, rows) == (self.width, self.rows) {
            return Ok(());
        }
        let old_top = self.top();
        let height = self.height.min(rows);
        let top = old_top.min(rows - height);
        self.width = width;
        self.rows = rows;
        self.height = height;
        self.free = rows - height - top;
        if old_top < rows {
            blank_below(&mut self.term, old_top)?;
        }
        scroll(&mut self.term, rows, old_top.min(rows).saturating_sub(top))?;
        // Resets both buffers: the next draw repaints every live row, stale width and all.
        self.term.resize(self.rect())
    }

    /// Growing takes the rows the host screen has free under the area, then scrolls for the rest:
    /// the new rows have to exist before anything is drawn into them. Shrinking blanks the rows it
    /// gives back, so the live content stays tight under the last committed block.
    pub fn set_height(&mut self, want: u16) -> io::Result<()> {
        let want = want.clamp(1, self.rows);
        if want == self.height {
            return Ok(());
        }
        if want > self.height {
            let need = want - self.height;
            let taken = need.min(self.free);
            self.free -= taken;
            scroll(&mut self.term, self.rows, need - taken)?;
            self.height = want;
        } else {
            self.free += self.height - want;
            self.height = want;
            let freed = self.top() + self.height;
            blank_below(&mut self.term, freed)?;
        }
        self.term.resize(self.rect())
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

    /// Pushes finished blocks into scrollback. Each line is written on the live area's top row;
    /// the area then slides down into a row a shrink freed, or, when it has none left and is
    /// already at the bottom of the screen, the host is scrolled and the row goes above it.
    ///
    /// Lines arrive pre-wrapped; nothing here wraps, so nothing here can clip.
    pub fn commit(&mut self, lines: Vec<Line<'static>>) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let width = self.width;
        for line in lines {
            let y = self.top();
            write_row(&mut self.term, line, y, width)?;
            if self.free > 0 {
                self.free -= 1;
            } else {
                scroll(&mut self.term, self.rows, 1)?;
            }
        }
        self.term.resize(self.rect())
    }

    pub fn clear_screen(&mut self) -> io::Result<()> {
        use crossterm::cursor::MoveTo;
        use crossterm::terminal::{Clear, ClearType};
        self.free = self.rows - self.height;
        queue!(self.term.backend_mut(), Clear(ClearType::All), MoveTo(0, 0))?;
        Backend::flush(self.term.backend_mut())?;
        self.term.resize(self.rect())
    }

    fn top(&self) -> u16 {
        self.rows - self.height - self.free
    }

    fn rect(&self) -> Rect {
        Rect::new(0, self.top(), self.width, self.height)
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
    Inline::new(
        io::stdout(),
        width,
        rows,
        height,
        cursor_row.min(rows.saturating_sub(1)),
    )
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

/// Blanks row `y` and everything under it. Only ever called on rows the live area owns.
fn blank_below<W: Write>(term: &mut Terminal<CrosstermBackend<W>>, y: u16) -> io::Result<()> {
    use crossterm::cursor::MoveTo;
    use crossterm::terminal::{Clear, ClearType};
    queue!(
        term.backend_mut(),
        MoveTo(0, y),
        Clear(ClearType::FromCursorDown)
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
