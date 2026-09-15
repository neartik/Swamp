//! A ratatui backend that never names an absolute screen row.
//!
//! The live area is a `Viewport::Fixed` rect at `y = 0..height`, so every row ratatui hands the
//! backend is already an offset inside the area. The backend tracks where the hardware cursor
//! sits inside that area and reaches any other row with `MoveUp`/`MoveDown` from there.
//!
//! That is the whole point. A window resize moves the area on screen - a host pulls rows back out
//! of scrollback when it grows, pushes them into it when it shrinks, and splits the rows a
//! narrower window cannot hold - but it moves the cursor with the row the cursor is on. The offset
//! therefore survives a resize, and the absolute row does not. Nothing here ever reads the cursor
//! back from the terminal.

use crossterm::cursor::{MoveDown, MoveTo, MoveToColumn, MoveUp};
use crossterm::queue;
use crossterm::style::{
    Attribute as CAttribute, Color as CColor, Colors, Print, SetAttribute, SetBackgroundColor,
    SetColors, SetForegroundColor,
};
use crossterm::terminal::{Clear, ClearType as CClearType};
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};
use std::cmp::Ordering;
use std::io::{self, Write};

pub struct RelativeBackend<W: Write> {
    inner: CrosstermBackend<W>,
    /// The hardware cursor's row inside the area, and its column on the screen.
    row: u16,
    col: u16,
    /// Rows the area owns. The cursor never leaves them.
    height: u16,
}

impl<W: Write> RelativeBackend<W> {
    pub fn new(out: W) -> RelativeBackend<W> {
        RelativeBackend {
            inner: CrosstermBackend::new(out),
            row: 0,
            col: 0,
            height: 1,
        }
    }

    pub fn row(&self) -> u16 {
        self.row
    }

    pub fn col(&self) -> u16 {
        self.col
    }

    /// Opens the area on the row the cursor is already on: the host's own output stopped there,
    /// and everything under it is the area's to take.
    pub fn open(&mut self, height: u16) -> io::Result<()> {
        queue!(
            self.inner,
            MoveToColumn(0),
            Clear(CClearType::FromCursorDown)
        )?;
        self.reset(height)
    }

    /// The area was moved by a window resize: it is found again `up` rows above the cursor, erased
    /// there, and reopened at that row with the new size. Nothing above that row is written to.
    pub fn reopen(&mut self, up: u16, height: u16) -> io::Result<()> {
        queue!(self.inner, MoveToColumn(0))?;
        if up > 0 {
            queue!(self.inner, MoveUp(up))?;
        }
        queue!(self.inner, Clear(CClearType::FromCursorDown))?;
        self.reset(height)
    }

    /// Everything gone, the area back at the top of the screen. The only absolute move in the
    /// chat, and a safe one: the screen it names has just been blanked.
    pub fn home(&mut self, height: u16) -> io::Result<()> {
        queue!(self.inner, Clear(CClearType::All), MoveTo(0, 0))?;
        self.reset(height)
    }

    /// Parks the cursor on row `y`, column `x`, of the area. The column move comes first: it also
    /// settles a pending wrap, which a bare vertical move would carry into the wrong row.
    pub fn goto(&mut self, x: u16, y: u16) -> io::Result<()> {
        let y = y.min(self.height.saturating_sub(1));
        queue!(self.inner, MoveToColumn(x))?;
        match y.cmp(&self.row) {
            Ordering::Less => queue!(self.inner, MoveUp(self.row - y))?,
            Ordering::Greater => queue!(self.inner, MoveDown(y - self.row))?,
            Ordering::Equal => {}
        }
        self.row = y;
        self.col = x;
        Ok(())
    }

    /// `n` more rows at the bottom, made by printing newlines on the *last* row of the area: the
    /// host scrolls when that row is the last on screen, and the cursor simply moves down when it
    /// is not. Either way the cursor ends `n` rows further from the top of the area, which is all
    /// this needs to know.
    pub fn grow(&mut self, n: u16) -> io::Result<()> {
        if n == 0 {
            return Ok(());
        }
        self.goto(0, self.height - 1)?;
        queue!(self.inner, Print("\n".repeat(n as usize)))?;
        self.height += n;
        self.row = self.height - 1;
        self.col = 0;
        Ok(())
    }

    /// Hands the last `n` rows back to the host, blanked, so the live content stays tight under
    /// the last committed block instead of floating over a hole.
    pub fn shrink(&mut self, n: u16) -> io::Result<()> {
        let keep = self.height.saturating_sub(n).max(1);
        if keep == self.height {
            return Ok(());
        }
        self.goto(0, keep)?;
        queue!(self.inner, Clear(CClearType::FromCursorDown))?;
        self.row = keep;
        self.height = keep;
        self.goto(0, keep - 1)
    }

    /// The top row leaves the area and a blank one joins at the bottom: one newline on the last
    /// row, which scrolls the host only when the area is already sitting on the last screen row.
    /// The cursor stays on the area's last row either way.
    pub fn slide(&mut self) -> io::Result<()> {
        self.goto(0, self.height - 1)?;
        queue!(self.inner, Print("\n"))?;
        self.col = 0;
        Ok(())
    }

    fn reset(&mut self, height: u16) -> io::Result<()> {
        self.row = 0;
        self.col = 0;
        self.height = 1;
        self.grow(height.saturating_sub(1))?;
        self.goto(0, 0)
    }
}

impl<W: Write> Backend for RelativeBackend<W> {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut fg = Color::Reset;
        let mut bg = Color::Reset;
        let mut modifier = Modifier::empty();
        let mut last: Option<(u16, u16)> = None;
        for (x, y, cell) in content {
            if !matches!(last, Some((px, py)) if x == px + 1 && y == py) {
                self.goto(x, y)?;
            }
            last = Some((x, y));
            if cell.modifier != modifier {
                queue_modifier(&mut self.inner, modifier, cell.modifier)?;
                modifier = cell.modifier;
            }
            if cell.fg != fg || cell.bg != bg {
                queue!(
                    self.inner,
                    SetColors(Colors::new(cell.fg.into(), cell.bg.into()))
                )?;
                fg = cell.fg;
                bg = cell.bg;
            }
            queue!(self.inner, Print(cell.symbol()))?;
            self.row = y.min(self.height.saturating_sub(1));
            self.col = x.saturating_add(1);
        }
        queue!(
            self.inner,
            SetForegroundColor(CColor::Reset),
            SetBackgroundColor(CColor::Reset),
            SetAttribute(CAttribute::Reset),
        )
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    /// The tracked position, never a cursor report: a `DSR` reply lands on the stdin the key
    /// `EventStream` is draining, and is answered late or not at all.
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(Position::new(self.col, self.row))
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let Position { x, y } = position.into();
        self.goto(x, y)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.clear_region(ClearType::AfterCursor)
    }

    /// Every crossterm clear is already relative to the cursor. `All` is not, and is downgraded:
    /// nothing above the cursor belongs to the chat.
    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(match clear_type {
            ClearType::All => ClearType::AfterCursor,
            other => other,
        })
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

/// The attribute changes between two cells, the way `CrosstermBackend` writes them.
fn queue_modifier<W: Write>(w: &mut W, from: Modifier, to: Modifier) -> io::Result<()> {
    let removed = from - to;
    if removed.contains(Modifier::REVERSED) {
        queue!(w, SetAttribute(CAttribute::NoReverse))?;
    }
    if removed.contains(Modifier::BOLD) {
        queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
        if to.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CAttribute::Dim))?;
        }
    }
    if removed.contains(Modifier::ITALIC) {
        queue!(w, SetAttribute(CAttribute::NoItalic))?;
    }
    if removed.contains(Modifier::UNDERLINED) {
        queue!(w, SetAttribute(CAttribute::NoUnderline))?;
    }
    if removed.contains(Modifier::DIM) {
        queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
    }
    if removed.contains(Modifier::CROSSED_OUT) {
        queue!(w, SetAttribute(CAttribute::NotCrossedOut))?;
    }
    if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
        queue!(w, SetAttribute(CAttribute::NoBlink))?;
    }
    let added = to - from;
    if added.contains(Modifier::REVERSED) {
        queue!(w, SetAttribute(CAttribute::Reverse))?;
    }
    if added.contains(Modifier::BOLD) {
        queue!(w, SetAttribute(CAttribute::Bold))?;
    }
    if added.contains(Modifier::ITALIC) {
        queue!(w, SetAttribute(CAttribute::Italic))?;
    }
    if added.contains(Modifier::UNDERLINED) {
        queue!(w, SetAttribute(CAttribute::Underlined))?;
    }
    if added.contains(Modifier::DIM) {
        queue!(w, SetAttribute(CAttribute::Dim))?;
    }
    if added.contains(Modifier::CROSSED_OUT) {
        queue!(w, SetAttribute(CAttribute::CrossedOut))?;
    }
    if added.contains(Modifier::SLOW_BLINK) {
        queue!(w, SetAttribute(CAttribute::SlowBlink))?;
    }
    if added.contains(Modifier::RAPID_BLINK) {
        queue!(w, SetAttribute(CAttribute::RapidBlink))?;
    }
    Ok(())
}
