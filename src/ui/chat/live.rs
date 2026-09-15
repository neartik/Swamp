use crossterm::queue;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::{Buffer, Cell};
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
/// Four rules keep committed rows safe:
///
/// - the area only grows by taking rows the host screen has free below it, and past that by
///   printing newlines on the *last* row, so the host scrolls committed rows up into real
///   scrollback. Nothing above the live area is ever cleared.
/// - the area holds live content and nothing else: a shrink hands the surplus rows straight back
///   to the host, blanked, so no hole is ever parked above the input bar.
/// - those handed-back rows are remembered as [`Inline::free`], and [`Inline::commit`] writes
///   finished lines into them before it scrolls, so no blank row is ever pushed into scrollback
///   between two committed blocks.
/// - a window resize moves the area on screen, so its place is never read back from the row it
///   had before: every write parks the cursor on a known row of the area, and [`Inline::reflow`]
///   finds the area again *relative* to that cursor.
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
    /// Blank rows between the last committed block and the top of the area: left there by a width
    /// change that put the area back on the last row of the screen, and the first rows
    /// [`Inline::commit`] writes into.
    gap: u16,
    /// Rows between the top of the area and the cursor the last write parked on it: what
    /// [`Inline::reflow`] measures the area's place from.
    cursor_off: u16,
    cursor_col: u16,
    /// The display width of every row of the last frame, read back from it: a host splits the
    /// rows it can no longer hold, so this is what says how many screen rows the area really has
    /// above its cursor.
    widths: Vec<u16>,
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
        let mut inline = Inline {
            term,
            height,
            free,
            width,
            rows,
            gap: 0,
            cursor_off: 0,
            cursor_col: 0,
            widths: Vec::new(),
        };
        inline.park()?;
        Ok(inline)
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    /// A window resize moves the live area: the host pulls rows back out of scrollback when it
    /// grows and pushes them into it when it shrinks, so the row the area had before the resize
    /// says nothing about where it is now. It is found again from the cursor the last write
    /// parked on it, erased there with one `Clear(FromCursorDown)`, and rebuilt at the row
    /// `probe` reads back. Nothing above that cursor is ever written to.
    ///
    /// `probe` is a cursor-position read, and is only ever answered when nothing else is draining
    /// stdin. When it fails the area is rebuilt on the last rows of the screen instead.
    pub fn reflow<F>(&mut self, width: u16, rows: u16, probe: F) -> io::Result<()>
    where
        F: FnOnce() -> Option<u16>,
    {
        use crossterm::cursor::{MoveToColumn, MoveUp};
        use crossterm::terminal::{Clear, ClearType};
        let up = self.rows_above(width);
        let back = self.term.backend_mut();
        queue!(back, MoveToColumn(0))?;
        if up > 0 {
            queue!(back, MoveUp(up))?;
        }
        queue!(back, Clear(ClearType::FromCursorDown))?;
        Backend::flush(back)?;
        self.widths.clear();
        let height = self.height.clamp(1, rows);
        let bottom = rows - height;
        let (top, short) = match probe() {
            Some(y) => (y.min(bottom), y.saturating_sub(bottom)),
            None => (bottom, 0),
        };
        // A new width splits the area's own rows and the host scrolls to fit them, so the blank
        // rows left under the cleared area are the area's own: it keeps the distance from the
        // bottom of the screen it had before instead of floating higher, and the rows it leaves
        // behind are the next commit's.
        let want = bottom.saturating_sub(self.free);
        let (top, gap) = if width != self.width && top < want {
            (want, want - top)
        } else {
            (top, 0)
        };
        self.gap = gap;
        self.width = width;
        self.rows = rows;
        self.height = height;
        self.free = rows - height - top;
        scroll(&mut self.term, rows, short)?;
        // Resets both buffers: the next draw repaints every live row, stale width and all.
        self.term.resize(self.rect())?;
        self.park()
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
            self.widths.truncate(want as usize);
        }
        self.term.resize(self.rect())?;
        self.park()
    }

    pub fn draw(&mut self, lines: Vec<Line<'static>>, cursor: (u16, u16)) -> io::Result<()> {
        let (row, col) = cursor;
        let mut widths = Vec::new();
        self.term.draw(|f| {
            let area = f.area();
            f.render_widget(Paragraph::new(lines), area);
            if row < area.height {
                f.set_cursor_position(Position::new(
                    area.x + col.min(area.width.saturating_sub(1)),
                    area.y + row,
                ));
            }
            widths = row_widths(f.buffer_mut());
        })?;
        // The frame is what the screen holds: `Terminal::resize` blanks the whole area, and every
        // draw after it writes every cell that changed, the ones that went empty included.
        self.widths = widths;
        self.cursor_off = row.min(self.height.saturating_sub(1));
        self.cursor_col = if row < self.height {
            col.min(self.width.saturating_sub(1))
        } else {
            0
        };
        let y = self.top() + self.cursor_off;
        queue!(
            self.term.backend_mut(),
            crossterm::cursor::MoveTo(self.cursor_col, y)
        )?;
        Backend::flush(self.term.backend_mut())
    }

    /// Screen rows between the top of the area and the cursor parked on it, once a host narrower
    /// than the frame was drawn at has split every row that no longer fits. A wider one splits
    /// nothing: the rows were drawn no wider than the area they are in.
    fn rows_above(&self, width: u16) -> u16 {
        if width >= self.width || width == 0 {
            return self.cursor_off;
        }
        let mut up = self.cursor_col / width;
        for i in 0..self.cursor_off as usize {
            up += self.widths.get(i).copied().unwrap_or(0).div_ceil(width).max(1);
        }
        up
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
            // The rows a re-anchor left blank above the area are filled first: the area does not
            // move, so nothing blank is ever pushed into scrollback ahead of the block.
            if self.gap > 0 {
                let y = self.top() - self.gap;
                write_row(&mut self.term, line, y, width)?;
                self.gap -= 1;
                continue;
            }
            let y = self.top();
            write_row(&mut self.term, line, y, width)?;
            // The area slid down a row: every row of it is one nearer its top than it was.
            if !self.widths.is_empty() {
                self.widths.remove(0);
            }
            if self.free > 0 {
                self.free -= 1;
            } else {
                scroll(&mut self.term, self.rows, 1)?;
            }
        }
        self.term.resize(self.rect())?;
        self.park()
    }

    pub fn clear_screen(&mut self) -> io::Result<()> {
        use crossterm::cursor::MoveTo;
        use crossterm::terminal::{Clear, ClearType};
        self.free = self.rows - self.height;
        self.gap = 0;
        self.widths.clear();
        queue!(self.term.backend_mut(), Clear(ClearType::All), MoveTo(0, 0))?;
        Backend::flush(self.term.backend_mut())?;
        self.term.resize(self.rect())?;
        self.park()
    }

    /// Leaves the cursor on the area's first row, where a resize can find the area whatever the
    /// host has done to the rows under it. Only [`Inline::draw`] parks it lower, on the caret.
    fn park(&mut self) -> io::Result<()> {
        use crossterm::cursor::MoveTo;
        self.cursor_off = 0;
        self.cursor_col = 0;
        let y = self.top();
        queue!(self.term.backend_mut(), MoveTo(0, y))?;
        Backend::flush(self.term.backend_mut())
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

/// The columns each row of a frame really uses: its last cell with something in it, the same
/// measure [`write_row`] takes of a committed row. A wide character owns two cells, so a count of
/// cells is a display width.
fn row_widths(buf: &Buffer) -> Vec<u16> {
    let width = buf.area.width as usize;
    if width == 0 {
        return Vec::new();
    }
    buf.content
        .chunks(width)
        .map(|row| {
            row.iter()
                .rposition(|cell| cell != &Cell::EMPTY)
                .map_or(0, |i| i as u16 + 1)
        })
        .collect()
}

/// The row is erased and then written up to its last cell with something in it, never padded to
/// the full width: a host splits any row too wide for a new window, and a padded one would split
/// into a committed row plus a blank one.
fn write_row<W: Write>(
    term: &mut Terminal<CrosstermBackend<W>>,
    line: Line<'static>,
    y: u16,
    width: u16,
) -> io::Result<()> {
    use crossterm::cursor::MoveTo;
    use crossterm::terminal::{Clear, ClearType};
    let rect = Rect::new(0, y, width, 1);
    let mut buffer = Buffer::empty(rect);
    Paragraph::new(line).render(rect, &mut buffer);
    let used = buffer
        .content
        .iter()
        .rposition(|cell| cell != &Cell::EMPTY)
        .map_or(0, |i| i + 1);
    queue!(
        term.backend_mut(),
        MoveTo(0, y),
        Clear(ClearType::UntilNewLine)
    )?;
    let cells = buffer.content[..used]
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
