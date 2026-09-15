mod relative;

use ratatui::backend::Backend;
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};
use relative::RelativeBackend;
use std::io::{self, Stdout, Write};

/// The live tail of the chat: the rows just under the last committed block, on the real screen,
/// never an alternate one. Everything above it is the terminal's own scrollback, selectable with
/// the mouse.
///
/// The area is a `Viewport::Fixed` rect at `y = 0..height` and the backend under it is
/// [`RelativeBackend`], so no row in this file is a screen row: they are all offsets inside the
/// area, and the backend reaches them from the cursor. Nothing ever reads the cursor back from
/// the terminal - not on a resize, not at startup - because a host answers a resize by moving
/// every row on screen, and any absolute row read around one is a race.
///
/// Three rules keep committed rows safe:
///
/// - the area only ever grows by printing newlines on its *last* row: the host scrolls committed
///   rows up into real scrollback if the area is already at the bottom of the screen, and the
///   cursor simply steps down into a free row if it is not. Nothing above the area is written to.
/// - the area holds live content and nothing else: a shrink hands the surplus rows straight back
///   to the host, blanked, so no hole is ever parked above the input bar.
/// - [`Inline::commit`] writes a finished line on the area's top row and then slides the area
///   down off it, so a block leaving the live area lands on rows it already occupied.
///
/// Every byte goes through the backend's writer, which is what lets the tests point one at a
/// terminal emulator.
pub struct Inline<W: Write> {
    term: Terminal<RelativeBackend<W>>,
    /// Rows the live content owns; it always fills them exactly.
    height: u16,
    width: u16,
    rows: u16,
    /// The display width of every row of the last frame, read back from it: a host splits the
    /// rows it can no longer hold, so this is what says how many screen rows the area really has
    /// above its cursor.
    widths: Vec<u16>,
}

impl<W: Write> Inline<W> {
    /// The area opens on the row the host's own output stopped on, at column 0.
    pub fn new(out: W, width: u16, rows: u16, height: u16) -> io::Result<Inline<W>> {
        let height = height.clamp(1, rows);
        let mut back = RelativeBackend::new(out);
        back.open(height)?;
        Backend::flush(&mut back)?;
        let term = Terminal::with_options(
            back,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, width, height)),
            },
        )?;
        let mut inline = Inline {
            term,
            height,
            width,
            rows,
            widths: Vec::new(),
        };
        inline.park()?;
        Ok(inline)
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    /// A window resize moves the live area, and the host moves the cursor with the row it is on,
    /// so the cursor is the only thing that still points at it. The area is found again `up` rows
    /// above that cursor, erased there with one `Clear(FromCursorDown)`, and reopened at exactly
    /// that row: it stays tight under the committed tail, and no blank row is left above it.
    pub fn reflow(&mut self, width: u16, rows: u16) -> io::Result<()> {
        let up = self.rows_above(width);
        let height = self.height.clamp(1, rows);
        let back = self.term.backend_mut();
        back.reopen(up, height)?;
        Backend::flush(back)?;
        self.widths.clear();
        self.width = width;
        self.rows = rows;
        self.height = height;
        // Resets both buffers: the next draw repaints every live row, stale width and all.
        self.term.resize(self.rect())?;
        self.park()
    }

    /// Growing prints a newline on the last row for each row it wants: the new rows have to exist
    /// before anything is drawn into them. Shrinking blanks the rows it gives back, so the live
    /// content stays tight under the last committed block.
    pub fn set_height(&mut self, want: u16) -> io::Result<()> {
        let want = want.clamp(1, self.rows);
        if want == self.height {
            return Ok(());
        }
        let back = self.term.backend_mut();
        if want > self.height {
            back.grow(want - self.height)?;
        } else {
            back.shrink(self.height - want)?;
            self.widths.truncate(want as usize);
        }
        Backend::flush(back)?;
        self.height = want;
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
                f.set_cursor_position(Position::new(col.min(area.width.saturating_sub(1)), row));
            }
            widths = row_widths(f.buffer_mut());
        })?;
        // The frame is what the screen holds: `Terminal::resize` blanks the whole area, and every
        // draw after it writes every cell that changed, the ones that went empty included.
        self.widths = widths;
        let off = row.min(self.height.saturating_sub(1));
        let col = if row < self.height {
            col.min(self.width.saturating_sub(1))
        } else {
            0
        };
        let back = self.term.backend_mut();
        back.goto(col, off)?;
        Backend::flush(back)
    }

    /// Screen rows between the top of the area and the cursor parked on it, once a host narrower
    /// than the frame was drawn at has split every row that no longer fits. A wider one splits
    /// nothing: the rows were drawn no wider than the area they are in.
    fn rows_above(&self, width: u16) -> u16 {
        let back = self.term.backend();
        let (row, col) = (back.row(), back.col());
        if width >= self.width || width == 0 {
            return row;
        }
        let mut up = col / width;
        for i in 0..row as usize {
            up += self.widths.get(i).copied().unwrap_or(0).div_ceil(width).max(1);
        }
        up
    }

    /// Pushes finished blocks into scrollback. Each line is written on the live area's top row;
    /// the area then slides down off that row, onto a row the host still has free below it or,
    /// when it has none, onto one the host scrolls up for.
    ///
    /// Lines arrive pre-wrapped; nothing here wraps, so nothing here can clip.
    pub fn commit(&mut self, lines: Vec<Line<'static>>) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let width = self.width;
        for line in lines {
            self.write_top(line, width)?;
            self.term.backend_mut().slide()?;
        }
        Backend::flush(self.term.backend_mut())?;
        self.widths.clear();
        self.term.resize(self.rect())?;
        self.park()
    }

    pub fn clear_screen(&mut self) -> io::Result<()> {
        self.widths.clear();
        let height = self.height;
        let back = self.term.backend_mut();
        back.home(height)?;
        Backend::flush(back)?;
        self.term.resize(self.rect())?;
        self.park()
    }

    /// The row is erased and then written up to its last cell with something in it, never padded
    /// to the full width: a host splits any row too wide for a new window, and a padded one would
    /// split into a committed row plus a blank one.
    fn write_top(&mut self, line: Line<'static>, width: u16) -> io::Result<()> {
        use ratatui::backend::ClearType;
        let rect = Rect::new(0, 0, width, 1);
        let mut buffer = Buffer::empty(rect);
        Paragraph::new(line).render(rect, &mut buffer);
        let used = buffer
            .content
            .iter()
            .rposition(|cell| cell != &Cell::EMPTY)
            .map_or(0, |i| i + 1);
        let back = self.term.backend_mut();
        back.goto(0, 0)?;
        back.clear_region(ClearType::UntilNewLine)?;
        let cells = buffer.content[..used]
            .iter()
            .enumerate()
            .map(|(i, cell)| (i as u16, 0, cell));
        back.draw(cells)
    }

    /// Leaves the cursor on the area's first row, where a resize can find the area whatever the
    /// host has done to the rows under it. Only [`Inline::draw`] parks it lower, on the caret.
    fn park(&mut self) -> io::Result<()> {
        let back = self.term.backend_mut();
        back.goto(0, 0)?;
        Backend::flush(back)
    }

    fn rect(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }
}

/// Raw mode and the kitty flags `shift+enter` needs. No cursor report: the area opens where the
/// shell left the cursor, and every row after that is relative to it.
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
    Inline::new(io::stdout(), width, rows, height)
}

/// The columns each row of a frame really uses: its last cell with something in it, the same
/// measure [`Inline::write_top`] takes of a committed row. A wide character owns two cells, so a
/// count of cells is a display width.
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
