use crate::ui::chat::theme::{Glyph, Role, Theme};
use crate::ui::fmt;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Text indent for an ordinary paragraph; the assistant bullet sits in the same two columns.
pub const INDENT: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListKind {
    Bullet,
    Ordered(u32),
}

#[derive(Debug, Clone, Default)]
struct BlockState {
    fence: Option<String>,
    lists: Vec<ListKind>,
    prev_blank: bool,
}

/// Line-oriented and incremental: a line is final the moment its newline arrives, because a
/// markdown block is decided by its opening, never by its closing.
#[derive(Debug, Clone)]
pub struct MdStream {
    pending: String,
    state: BlockState,
    width: u16,
    theme: Theme,
    any: bool,
}

impl MdStream {
    pub fn new(width: u16, theme: Theme) -> Self {
        MdStream {
            pending: String::new(),
            state: BlockState::default(),
            width: width.max(8),
            theme,
            any: false,
        }
    }

    pub fn set_width(&mut self, width: u16) {
        self.width = width.max(8);
    }

    pub fn is_empty(&self) -> bool {
        !self.any && self.pending.is_empty()
    }

    /// Everything up to the last newline is final and ready to commit.
    pub fn push(&mut self, delta: &str) -> Vec<Line<'static>> {
        self.pending.push_str(delta);
        let Some(last) = self.pending.rfind('\n') else {
            return Vec::new();
        };
        let complete: String = self.pending.drain(..=last).collect();
        let mut out = Vec::new();
        for src in complete.trim_end_matches('\n').split('\n') {
            let mut state = std::mem::take(&mut self.state);
            out.extend(self.line(&mut state, src));
            self.state = state;
        }
        self.any |= !out.is_empty();
        out
    }

    /// The incomplete trailing line, re-parsed every frame. Never final, never mutating.
    pub fn tail(&self) -> Vec<Line<'static>> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let mut state = self.state.clone();
        self.line(&mut state, &self.pending)
    }

    pub fn finish(&mut self) -> Vec<Line<'static>> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let rest = std::mem::take(&mut self.pending);
        let mut state = std::mem::take(&mut self.state);
        let out = self.line(&mut state, &rest);
        self.state = state;
        self.any |= !out.is_empty();
        out
    }

    fn line(&self, st: &mut BlockState, raw: &str) -> Vec<Line<'static>> {
        let src = fmt::sanitize(raw.trim_end_matches('\r'));
        let body = src.trim_start();
        let lead = src.len() - body.len();
        let width = self.width;
        let t = &self.theme;

        if let Some(open) = st.fence.clone() {
            if body.starts_with("```") {
                st.fence = None;
                return Vec::new();
            }
            let _ = open;
            st.prev_blank = false;
            return vec![Line::from(t.span(
                format!("    {}", fmt::truncate(&src, width.saturating_sub(4) as usize)),
                Role::Code,
            ))];
        }
        if let Some(lang) = body.strip_prefix("```") {
            st.fence = Some(lang.trim().to_owned());
            st.prev_blank = false;
            if lang.trim().is_empty() {
                return Vec::new();
            }
            return vec![Line::from(Span::styled(
                format!("  {}", lang.trim()),
                t.style(Role::Meta).add_modifier(Modifier::DIM),
            ))];
        }
        if body.is_empty() {
            if st.prev_blank {
                return Vec::new();
            }
            st.prev_blank = true;
            st.lists.clear();
            return vec![Line::from(String::new())];
        }
        if is_rule(body) {
            st.prev_blank = false;
            return vec![Line::from(t.span(
                format!(
                    "  {}",
                    t.g(Glyph::Rule).repeat(width.saturating_sub(4) as usize)
                ),
                Role::Meta,
            ))];
        }
        if let Some(rest) = heading(body) {
            let blank = !st.prev_blank;
            st.prev_blank = false;
            let mut out = Vec::new();
            if blank {
                out.push(Line::from(String::new()));
            }
            let mut spans = vec![t.span("  ", Role::Text)];
            spans.extend(inline(&rest, t, Role::Name));
            out.extend(wrap_spans(spans, width, INDENT));
            return out;
        }
        if let Some(rest) = body.strip_prefix("> ").or_else(|| {
            if body == ">" {
                Some("")
            } else {
                None
            }
        }) {
            st.prev_blank = false;
            let mut spans = vec![t.span(format!("  {} ", t.g(Glyph::Quote)), Role::Meta)];
            let mut quoted = inline(rest, t, Role::Meta);
            for s in &mut quoted {
                s.style = s.style.add_modifier(Modifier::ITALIC);
            }
            spans.extend(quoted);
            return wrap_spans(spans, width, INDENT + 2);
        }
        let depth = (lead / 2) as u16;
        if let Some(rest) = bullet_item(body) {
            st.prev_blank = false;
            st.lists.push(ListKind::Bullet);
            let pad = " ".repeat((INDENT + depth * 2) as usize);
            let mut spans = vec![
                t.span(pad, Role::Text),
                t.span(format!("{} ", t.g(Glyph::ListDot)), Role::Accent),
            ];
            spans.extend(inline(rest, t, Role::Text));
            return wrap_spans(spans, width, INDENT + depth * 2 + 2);
        }
        if let Some((marker, rest)) = ordered_item(body) {
            st.prev_blank = false;
            st.lists.push(ListKind::Ordered(0));
            let pad = " ".repeat((INDENT + depth * 2) as usize);
            let mut spans = vec![
                t.span(pad, Role::Text),
                t.span(format!("{marker} "), Role::Meta),
            ];
            spans.extend(inline(rest, t, Role::Text));
            return wrap_spans(
                spans,
                width,
                INDENT + depth * 2 + marker.width() as u16 + 1,
            );
        }
        st.prev_blank = false;
        let mut spans = vec![t.span(" ".repeat(INDENT as usize), Role::Text)];
        spans.extend(inline(&src, t, Role::Text));
        wrap_spans(spans, width, INDENT)
    }
}

fn is_rule(s: &str) -> bool {
    let t = s.trim();
    (t.len() >= 3 && t.chars().all(|c| c == '-')) || (t.len() >= 3 && t.chars().all(|c| c == '*'))
}

fn heading(s: &str) -> Option<String> {
    let hashes = s.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = s[hashes..].strip_prefix(' ')?;
    Some(rest.to_owned())
}

fn bullet_item(s: &str) -> Option<&str> {
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = s.strip_prefix(m) {
            return Some(rest);
        }
    }
    None
}

fn ordered_item(s: &str) -> Option<(String, &str)> {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    let rest = s[digits.len()..].strip_prefix(". ")?;
    Some((format!("{digits}."), rest))
}

/// One left-to-right scan, no backtracking: an unmatched marker is emitted literally, so a
/// half-arrived `**bold` reads as text and fixes itself when the closer lands.
pub fn inline(src: &str, t: &Theme, base: Role) -> Vec<Span<'static>> {
    let chars: Vec<char> = src.chars().collect();
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    while i < chars.len() {
        match token(&chars, i, t, base) {
            Some((spans, next)) => {
                if !plain.is_empty() {
                    out.push(t.span(std::mem::take(&mut plain), base));
                }
                out.extend(spans);
                i = next;
            }
            None => {
                plain.push(chars[i]);
                i += 1;
            }
        }
    }
    if !plain.is_empty() {
        out.push(t.span(plain, base));
    }
    out
}

/// One inline construct at `i`, and where it ends. `None` means the character is literal.
fn token(chars: &[char], i: usize, t: &Theme, base: Role) -> Option<(Vec<Span<'static>>, usize)> {
    let c = chars[i];
    if c == '`' {
        let end = find(chars, i + 1, "`")?;
        let body: String = chars[i + 1..end].iter().collect();
        // The padding only earns its columns against a background; without one it is stray space.
        let text = if t.style(Role::Code).bg.is_some() {
            format!(" {body} ")
        } else {
            body
        };
        return Some((vec![t.span(text, Role::Code)], end + 1));
    }
    for (marker, modifier) in [
        ("**", Modifier::BOLD),
        ("__", Modifier::BOLD),
        ("~~", Modifier::CROSSED_OUT),
    ] {
        if starts(chars, i, marker)
            && let Some(end) = find(chars, i + 2, marker)
        {
            let body: String = chars[i + 2..end].iter().collect();
            return Some((
                vec![Span::styled(body, t.style(base).add_modifier(modifier))],
                end + marker.len(),
            ));
        }
    }
    if (c == '*' || c == '_') && !starts(chars, i, "**") && !starts(chars, i, "__") {
        let end = find(chars, i + 1, &c.to_string())?;
        if end == i + 1 {
            return None;
        }
        let body: String = chars[i + 1..end].iter().collect();
        return Some((
            vec![Span::styled(
                body,
                t.style(base).add_modifier(Modifier::ITALIC),
            )],
            end + 1,
        ));
    }
    if c == '[' {
        let close = find(chars, i + 1, "]")?;
        if !starts(chars, close + 1, "(") {
            return None;
        }
        let paren = find(chars, close + 2, ")")?;
        let text: String = chars[i + 1..close].iter().collect();
        let url: String = chars[close + 2..paren].iter().collect();
        let mut spans = vec![Span::styled(
            text.clone(),
            t.style(base).add_modifier(Modifier::UNDERLINED),
        )];
        if !text.contains(&url) {
            spans.push(t.span(format!(" ({url})"), Role::Meta));
        }
        return Some((spans, paren + 1));
    }
    None
}

fn starts(chars: &[char], at: usize, marker: &str) -> bool {
    let m: Vec<char> = marker.chars().collect();
    chars.len() >= at + m.len() && chars[at..at + m.len()] == m[..]
}

fn find(chars: &[char], from: usize, marker: &str) -> Option<usize> {
    let m: Vec<char> = marker.chars().collect();
    if chars.len() < m.len() {
        return None;
    }
    (from..=chars.len() - m.len()).find(|i| chars[*i..*i + m.len()] == m[..])
}

/// Greedy, span-aware and width-aware: a CJK column counts two, and a token longer than the
/// line is hard split rather than allowed to overflow.
pub fn wrap_spans(spans: Vec<Span<'static>>, width: u16, indent: u16) -> Vec<Line<'static>> {
    let width = (width as usize).max(indent as usize + 4);
    let pad = " ".repeat(indent as usize);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    let mut start = 0usize;

    for (text, style, space) in tokenize(spans) {
        let w = text.width();
        if used + w <= width {
            used += w;
            cur.push(Span::styled(text, style));
            continue;
        }
        if space {
            lines.push(Line::from(std::mem::take(&mut cur)));
            cur.push(Span::raw(pad.clone()));
            used = indent as usize;
            start = used;
            continue;
        }
        if used > start {
            lines.push(Line::from(std::mem::take(&mut cur)));
            cur.push(Span::raw(pad.clone()));
            used = indent as usize;
            start = used;
        }
        if used + w <= width {
            used += w;
            cur.push(Span::styled(text, style));
            continue;
        }
        // Longer than a whole line even alone: split it at the last column that fits.
        let mut chunk = String::new();
        for c in text.chars() {
            let cw = c.to_string().width();
            if used + cw > width {
                cur.push(Span::styled(std::mem::take(&mut chunk), style));
                lines.push(Line::from(std::mem::take(&mut cur)));
                cur.push(Span::raw(pad.clone()));
                used = indent as usize;
                start = used;
            }
            chunk.push(c);
            used += cw;
        }
        if !chunk.is_empty() {
            cur.push(Span::styled(chunk, style));
        }
    }
    if !cur.is_empty() {
        lines.push(Line::from(cur));
    }
    if lines.is_empty() {
        lines.push(Line::from(String::new()));
    }
    lines
}

/// Runs of spaces and runs of non-spaces, each carrying its span's style.
fn tokenize(spans: Vec<Span<'static>>) -> Vec<(String, ratatui::style::Style, bool)> {
    let mut out = Vec::new();
    for span in spans {
        let style = span.style;
        let mut run = String::new();
        let mut space = false;
        for c in span.content.chars() {
            let is_space = c == ' ';
            if !run.is_empty() && is_space != space {
                out.push((std::mem::take(&mut run), style, space));
            }
            space = is_space;
            run.push(c);
        }
        if !run.is_empty() {
            out.push((run, style, space));
        }
    }
    out
}

/// The text of a rendered line, for tests and for the ctrl+o expansion block.
pub fn text_of(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md() -> MdStream {
        MdStream::new(60, Theme::plain())
    }

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(text_of).collect()
    }

    #[test]
    fn a_document_renders_the_same_byte_by_byte_as_all_at_once() {
        let doc = "# Title\n\nsome *text* with `code` and a [link](https://x.y)\n\n- one\n- two\n\n\
                   1. first\n2. second\n\n```rust\nlet x = 1;\n```\n\n> quoted\n\n---\n";
        let mut whole = md();
        let bulk = whole.push(doc);
        let mut drip = md();
        let mut trickle = Vec::new();
        for b in doc.chars() {
            trickle.extend(drip.push(&b.to_string()));
        }
        assert_eq!(texts(&bulk), texts(&trickle));
    }

    #[test]
    fn a_fence_split_across_deltas_never_parses_its_body_as_markdown() {
        let mut s = md();
        assert!(s.push("```ru").is_empty());
        let open = s.push("st\n");
        assert_eq!(texts(&open), vec!["  rust".to_owned()]);
        let body = s.push("**not bold**\n");
        assert_eq!(texts(&body), vec!["    **not bold**".to_owned()]);
        let close = s.push("```\n");
        assert!(close.is_empty());
    }

    #[test]
    fn a_fence_header_is_dim_and_a_bare_fence_has_no_header_at_all() {
        let mut s = md();
        let open = s.push("```python\n");
        assert_eq!(texts(&open), vec!["  python".to_owned()]);
        assert!(
            open[0].spans[0].style.add_modifier.contains(Modifier::DIM),
            "{:?}",
            open[0].spans[0].style
        );
        assert_eq!(texts(&s.push("print(1)\n")), vec!["    print(1)".to_owned()]);
        let mut s = md();
        assert!(s.push("```\n").is_empty(), "nothing to label");
    }

    #[test]
    fn inline_code_is_padded_only_where_the_palette_paints_a_background() {
        let mut plain = md();
        assert_eq!(
            texts(&plain.push("run `calc.py` now\n")),
            vec!["  run calc.py now".to_owned()],
            "NO_COLOR gets no stray spaces"
        );
        let mut painted = MdStream::new(
            60,
            Theme {
                palette: crate::ui::chat::theme::Palette::TrueColor,
                ascii: false,
            },
        );
        assert_eq!(
            texts(&painted.push("run `calc.py` now\n")),
            vec!["  run  calc.py  now".to_owned()]
        );
    }

    #[test]
    fn an_unterminated_fence_still_finishes() {
        let mut s = md();
        s.push("```\nline\n");
        let rest = s.finish();
        assert!(rest.is_empty() || texts(&rest)[0].contains("line"));
    }

    #[test]
    fn an_unmatched_marker_is_literal_and_fixes_itself_when_it_closes() {
        let mut s = md();
        s.push("**bold");
        assert_eq!(texts(&s.tail()), vec!["  **bold".to_owned()]);
        s.push(" here**");
        assert_eq!(texts(&s.tail()), vec!["  bold here".to_owned()]);
    }

    #[test]
    fn a_cjk_paragraph_wraps_on_display_width_not_char_count() {
        let mut s = MdStream::new(20, Theme::plain());
        let lines = s.push("日本語のテキストはここで折り返されるはずです\n");
        for l in &lines {
            assert!(text_of(l).width() <= 20, "{:?}", text_of(l));
        }
        assert!(lines.len() > 1);
    }

    #[test]
    fn headings_lists_and_quotes_get_their_markers() {
        let mut s = md();
        let out = texts(&s.push("## Heading\n- item\n1. first\n> quoted\n"));
        assert!(out.iter().any(|l| l == "  Heading"), "{out:?}");
        assert!(out.iter().any(|l| l == "  • item"), "{out:?}");
        assert!(out.iter().any(|l| l == "  1. first"), "{out:?}");
        assert!(out.iter().any(|l| l.contains("│ quoted")), "{out:?}");
    }

    #[test]
    fn control_characters_never_survive_a_delta() {
        let mut s = md();
        let out = texts(&s.push("\u{1b}[2Jfake banner\n"));
        assert!(!out[0].contains('\u{1b}'), "{out:?}");
        assert!(out[0].contains("fake banner"));
    }
}
