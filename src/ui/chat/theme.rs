use crate::model::core::NodeState;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Palette {
    TrueColor,
    Ansi256,
    Plain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Accent,
    Ok,
    Err,
    Meta,
    Name,
    Code,
    UserBar,
    TierHi,
    Run,
    Text,
}

/// Which glyph a row asks for; the ascii column is what a non-UTF-8 locale gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    Star,
    Bullet,
    Connector,
    Detail,
    Succeeded,
    Failed,
    Cancelled,
    Blocked,
    Orphaned,
    Queued,
    Leased,
    Mode,
    TokenArrow,
    Rule,
    Caret,
    Select,
    BoxTopLeft,
    BoxTopRight,
    BoxBottomLeft,
    BoxBottomRight,
    BoxVertical,
    Quote,
    ListDot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub palette: Palette,
    pub ascii: bool,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            palette: Palette::Plain,
            ascii: false,
        }
    }
}

impl Theme {
    /// `NO_COLOR` and `--no-color` win; `Plain` is never guessed from a terminal probe.
    pub fn detect(color: bool, chat_theme: Option<&str>) -> Theme {
        let ascii = !utf8_locale();
        let no_color = std::env::var("NO_COLOR").is_ok_and(|v| !v.is_empty());
        if !color || no_color {
            return Theme {
                palette: Palette::Plain,
                ascii,
            };
        }
        if let Some(name) = chat_theme {
            match name.trim().to_ascii_lowercase().as_str() {
                "plain" => {
                    return Theme {
                        palette: Palette::Plain,
                        ascii,
                    };
                }
                "ansi256" => {
                    return Theme {
                        palette: Palette::Ansi256,
                        ascii,
                    };
                }
                "truecolor" => {
                    return Theme {
                        palette: Palette::TrueColor,
                        ascii,
                    };
                }
                _ => {}
            }
        }
        let truecolor = std::env::var("COLORTERM")
            .is_ok_and(|v| v.eq_ignore_ascii_case("truecolor") || v.eq_ignore_ascii_case("24bit"));
        Theme {
            palette: if truecolor {
                Palette::TrueColor
            } else {
                Palette::Ansi256
            },
            ascii,
        }
    }

    pub fn plain() -> Theme {
        Theme::default()
    }

    pub fn style(&self, r: Role) -> Style {
        let base = Style::default();
        match (self.palette, r) {
            (Palette::Plain, Role::Err) => base.add_modifier(Modifier::BOLD),
            (Palette::Plain, Role::Meta | Role::Run | Role::Code) => {
                base.add_modifier(Modifier::DIM)
            }
            (Palette::Plain, Role::Name) => base.add_modifier(Modifier::BOLD),
            (Palette::Plain, _) => base,
            (_, Role::Name) => base.add_modifier(Modifier::BOLD),
            (Palette::TrueColor, Role::Accent) => base.fg(Color::Rgb(215, 119, 87)),
            (Palette::TrueColor, Role::Ok) => base.fg(Color::Rgb(87, 170, 120)),
            (Palette::TrueColor, Role::Err) => base.fg(Color::Rgb(214, 90, 90)),
            (Palette::TrueColor, Role::Meta | Role::Run) => base.fg(Color::Rgb(136, 136, 136)),
            (Palette::TrueColor, Role::Code) => base
                .fg(Color::Rgb(199, 182, 158))
                .bg(Color::Rgb(38, 38, 38)),
            (Palette::TrueColor, Role::UserBar) => base.bg(Color::Rgb(38, 38, 40)),
            (Palette::TrueColor, Role::TierHi) => base.fg(Color::Rgb(96, 150, 180)),
            (Palette::TrueColor, Role::Text) => base,
            (Palette::Ansi256, Role::Accent) => base.fg(Color::Indexed(173)),
            (Palette::Ansi256, Role::Ok) => base.fg(Color::Indexed(71)),
            (Palette::Ansi256, Role::Err) => base.fg(Color::Indexed(167)),
            (Palette::Ansi256, Role::Meta | Role::Run) => base.fg(Color::Indexed(245)),
            (Palette::Ansi256, Role::Code) => base.fg(Color::Indexed(180)).bg(Color::Indexed(235)),
            (Palette::Ansi256, Role::UserBar) => base.bg(Color::Indexed(236)),
            (Palette::Ansi256, Role::TierHi) => base.fg(Color::Indexed(74)),
            (Palette::Ansi256, Role::Text) => base,
        }
    }

    pub fn span<S: Into<String>>(&self, text: S, r: Role) -> Span<'static> {
        Span::styled(text.into(), self.style(r))
    }

    pub fn g(&self, g: Glyph) -> &'static str {
        match (g, self.ascii) {
            (Glyph::Star, false) => "✻",
            (Glyph::Star, true) => "*",
            (Glyph::Bullet, false) => "●",
            (Glyph::Bullet, true) => "*",
            (Glyph::Connector, false) => "⎿",
            (Glyph::Connector, true) => "-",
            (Glyph::Detail, false) => "└ ",
            (Glyph::Detail, true) => "\\ ",
            (Glyph::Succeeded, false) => "✔",
            (Glyph::Succeeded, true) => "+",
            (Glyph::Failed, false) => "✘",
            (Glyph::Failed, true) => "x",
            (Glyph::Cancelled, false) => "⊘",
            (Glyph::Cancelled, true) => "/",
            (Glyph::Blocked, false) => "⏸",
            (Glyph::Blocked, true) => "~",
            (Glyph::Orphaned, _) => "?",
            (Glyph::Queued, false) => "·",
            (Glyph::Queued, true) => ".",
            (Glyph::Leased, false) => "◦",
            (Glyph::Leased, true) => "o",
            (Glyph::Mode, false) => "⏵⏵",
            (Glyph::Mode, true) => ">>",
            (Glyph::TokenArrow, false) => "↓",
            (Glyph::TokenArrow, true) => "v",
            (Glyph::Rule, false) => "─",
            (Glyph::Rule, true) => "-",
            (Glyph::Caret, _) => "█",
            (Glyph::Select, false) => "▌",
            (Glyph::Select, true) => "|",
            (Glyph::BoxTopLeft, false) => "╭",
            (Glyph::BoxTopRight, false) => "╮",
            (Glyph::BoxBottomLeft, false) => "╰",
            (Glyph::BoxBottomRight, false) => "╯",
            (Glyph::BoxTopLeft | Glyph::BoxTopRight, true) => "+",
            (Glyph::BoxBottomLeft | Glyph::BoxBottomRight, true) => "+",
            (Glyph::BoxVertical, false) => "│",
            (Glyph::BoxVertical, true) => "|",
            (Glyph::Quote, false) => "│",
            (Glyph::Quote, true) => ">",
            (Glyph::ListDot, false) => "•",
            (Glyph::ListDot, true) => "-",
        }
    }

    /// The glyph and the role one worker row wears, outside of the running animation.
    pub fn state_glyph(&self, s: &NodeState) -> &'static str {
        match s {
            NodeState::Succeeded => self.g(Glyph::Succeeded),
            NodeState::Failed { .. } => self.g(Glyph::Failed),
            NodeState::Cancelled { .. } => self.g(Glyph::Cancelled),
            NodeState::Orphaned { .. } => self.g(Glyph::Orphaned),
            NodeState::Queued | NodeState::Blocked { .. } => self.g(Glyph::Queued),
            NodeState::Leased { .. } => self.g(Glyph::Leased),
            NodeState::Running { .. } => self.g(Glyph::Queued),
        }
    }

    pub fn state_role(&self, s: &NodeState) -> Role {
        match s {
            NodeState::Succeeded => Role::Ok,
            NodeState::Failed { .. } => Role::Err,
            NodeState::Running { .. } => Role::Run,
            _ => Role::Meta,
        }
    }

    pub fn state_style(&self, s: &NodeState) -> Style {
        self.style(self.state_role(s))
    }
}

fn utf8_locale() -> bool {
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(v) = std::env::var(key)
            && !v.is_empty()
        {
            return v.to_ascii_uppercase().contains("UTF-8")
                || v.to_ascii_uppercase().contains("UTF8");
        }
    }
    // A terminal with no locale at all is the CI case, where UTF-8 is the safe guess.
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_is_never_guessed_but_no_color_forces_it() {
        let t = Theme::detect(false, None);
        assert_eq!(t.palette, Palette::Plain);
        let t = Theme::detect(true, Some("ansi256"));
        assert_eq!(t.palette, Palette::Ansi256);
        let t = Theme::detect(true, Some("truecolor"));
        assert_eq!(t.palette, Palette::TrueColor);
    }

    #[test]
    fn the_accent_and_ok_roles_carry_the_documented_rgb() {
        let t = Theme {
            palette: Palette::TrueColor,
            ascii: false,
        };
        assert_eq!(t.style(Role::Accent).fg, Some(Color::Rgb(215, 119, 87)));
        assert_eq!(t.style(Role::Ok).fg, Some(Color::Rgb(87, 170, 120)));
    }
}
