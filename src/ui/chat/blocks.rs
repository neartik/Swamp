use crate::ui::chat::markdown::MdStream;
use crate::ui::chat::theme::{Glyph, Role, Theme};
use crate::ui::chat::workers::Batch;
use crate::ui::chat::{markdown, spinner};
use crate::ui::fmt;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// The widest a welcome box gets, however wide the terminal is.
const BOX_MAX: u16 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Ok,
    Failed,
}

#[derive(Debug, Clone, Default)]
pub struct WelcomeInfo {
    pub cwd: String,
    pub brain: String,
    pub workers: String,
    pub run: String,
}

#[derive(Debug, Clone)]
pub enum Block {
    Welcome(WelcomeInfo),
    User(String),
    Assistant {
        md: MdStream,
        started: bool,
        done: bool,
    },
    Thinking {
        md: MdStream,
        started: bool,
    },
    Tool {
        id: String,
        name: String,
        preview: String,
        state: ToolState,
        result: Vec<String>,
        expanded: bool,
    },
    Dispatch(Box<Batch>),
    Slash {
        title: String,
        body: Vec<String>,
    },
    Notice {
        glyph: String,
        role: Role,
        head: String,
        body: Vec<String>,
    },
}

/// Everything a block needs to draw itself, live or committed.
pub struct Ctx<'a> {
    pub width: u16,
    pub theme: &'a Theme,
    pub collapse: usize,
    pub tick: u64,
    pub live: bool,
}

impl Block {
    pub fn render(&self, cx: &Ctx<'_>) -> Vec<Line<'static>> {
        let t = cx.theme;
        match self {
            Block::Welcome(info) => welcome(info, cx),
            Block::User(text) => user_bar(text, cx),
            Block::Assistant { md, started, .. } => {
                stream_tail(md, *started, cx, t.g(Glyph::Bullet), Role::Accent, false)
            }
            Block::Thinking { md, started } => {
                stream_tail(md, *started, cx, t.g(Glyph::Star), Role::Meta, true)
            }
            Block::Tool {
                name,
                preview,
                state,
                result,
                expanded,
                ..
            } => tool(name, preview, *state, result, *expanded, cx),
            Block::Dispatch(batch) => batch.render(cx.width, t, cx.tick, !cx.live),
            Block::Slash { title, body } => {
                let mut out = Vec::new();
                if !title.is_empty() {
                    out.push(Line::from(t.span(title.clone(), Role::Name)));
                }
                out.extend(
                    body.iter()
                        .map(|l| Line::from(t.span(clip(l, cx.width), Role::Meta))),
                );
                out
            }
            Block::Notice {
                glyph,
                role,
                head,
                body,
            } => notice(glyph, *role, head, body, cx),
        }
    }

    /// Whether ctrl+o has anything to toggle here.
    pub fn collapsible(&self, collapse: usize) -> bool {
        match self {
            Block::Tool { result, .. } => result.len() > collapse,
            Block::Dispatch(b) => b.rows.len() > crate::ui::chat::workers::MAX_ROWS,
            _ => false,
        }
    }

    pub fn toggle(&mut self) {
        match self {
            Block::Tool { expanded, .. } => *expanded = !*expanded,
            Block::Dispatch(b) => b.expanded = !b.expanded,
            _ => {}
        }
    }
}

/// The very first rendered line of a stream wears the bullet in the same two columns the
/// paragraph indent occupies.
pub fn bullet_first(lines: &mut [Line<'static>], t: &Theme, glyph: &str, role: Role) {
    let Some(first) = lines.first_mut() else {
        return;
    };
    let Some(span) = first.spans.first_mut() else {
        return;
    };
    let content = span.content.to_string();
    let rest = content.strip_prefix("  ").unwrap_or(&content).to_owned();
    span.content = rest.into();
    first
        .spans
        .insert(0, Span::styled(format!("{glyph} "), t.style(role)));
}

fn stream_tail(
    md: &MdStream,
    started: bool,
    cx: &Ctx<'_>,
    glyph: &str,
    role: Role,
    italic: bool,
) -> Vec<Line<'static>> {
    let t = cx.theme;
    let mut lines = md.tail();
    if italic {
        for l in &mut lines {
            for s in &mut l.spans {
                s.style = s.style.add_modifier(Modifier::ITALIC);
            }
        }
    }
    if lines.is_empty() && started {
        return Vec::new();
    }
    if lines.is_empty() {
        lines.push(Line::from(t.span("  ", Role::Text)));
    }
    if !started {
        bullet_first(&mut lines, t, glyph, role);
    }
    if cx.live && let Some(last) = lines.last_mut() {
        last.spans.push(t.span(t.g(Glyph::Caret), Role::Meta));
    }
    lines
}

fn welcome(info: &WelcomeInfo, cx: &Ctx<'_>) -> Vec<Line<'static>> {
    let t = cx.theme;
    let width = cx.width.min(BOX_MAX);
    let inner = width.saturating_sub(4) as usize;
    let rule = t.g(Glyph::Rule).repeat(width.saturating_sub(2) as usize);
    let mut out = vec![Line::from(t.span(
        format!("{}{rule}{}", t.g(Glyph::BoxTopLeft), t.g(Glyph::BoxTopRight)),
        Role::Meta,
    ))];
    let mut row = |spans: Vec<Span<'static>>| {
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        let mut line = vec![t.span(format!("{} ", t.g(Glyph::BoxVertical)), Role::Meta)];
        line.extend(spans);
        line.push(Span::raw(" ".repeat(inner.saturating_sub(used))));
        line.push(t.span(format!(" {}", t.g(Glyph::BoxVertical)), Role::Meta));
        out.push(Line::from(line));
    };
    row(vec![
        t.span(format!("{} ", t.g(Glyph::Star)), Role::Accent),
        Span::styled(
            "Welcome to Swamp".to_owned(),
            t.style(Role::Accent).add_modifier(Modifier::BOLD),
        ),
    ]);
    row(Vec::new());
    row(vec![t.span(
        "  /help for commands, /status for the run tree".to_owned(),
        Role::Meta,
    )]);
    for (label, value, role) in [
        ("  cwd: ", info.cwd.clone(), Role::Meta),
        ("  brain: ", info.brain.clone(), Role::Name),
        ("  workers: ", info.workers.clone(), Role::Meta),
        ("  run: ", info.run.clone(), Role::Name),
    ] {
        if value.is_empty() {
            continue;
        }
        let room = inner.saturating_sub(label.width());
        row(vec![
            t.span(label, Role::Meta),
            t.span(fmt::truncate(&value, room), role),
        ]);
    }
    out.push(Line::from(t.span(
        format!(
            "{}{rule}{}",
            t.g(Glyph::BoxBottomLeft),
            t.g(Glyph::BoxBottomRight)
        ),
        Role::Meta,
    )));
    out.push(Line::from(String::new()));
    out
}

/// One full-width row so the echoed prompt reads as a bar, not as a quoted line.
fn user_bar(text: &str, cx: &Ctx<'_>) -> Vec<Line<'static>> {
    let t = cx.theme;
    let width = cx.width as usize;
    let body = fmt::truncate(text, width.saturating_sub(2));
    let pad = width.saturating_sub(2 + body.width());
    let bar = t.style(Role::UserBar);
    vec![Line::from(vec![
        Span::styled("> ".to_owned(), bar.patch(t.style(Role::Meta))),
        Span::styled(body, bar),
        Span::styled(" ".repeat(pad), bar),
    ])]
}

fn tool(
    name: &str,
    preview: &str,
    state: ToolState,
    result: &[String],
    expanded: bool,
    cx: &Ctx<'_>,
) -> Vec<Line<'static>> {
    let t = cx.theme;
    let role = match state {
        ToolState::Running => Role::Run,
        ToolState::Ok => Role::Ok,
        ToolState::Failed => Role::Err,
    };
    let short = tool_args::short_name(name);
    let arg = tool_args::preview(name, preview, cx.width);
    let head = Line::from(vec![
        t.span(format!("{} ", t.g(Glyph::Bullet)), role),
        Span::styled(short.clone(), t.style(Role::Name)),
        t.span(format!("({arg})"), Role::Meta),
    ]);
    let mut out = vec![head];
    let room = cx.width.saturating_sub(5) as usize;
    if state == ToolState::Running {
        out.push(Line::from(vec![
            t.span(format!("  {}  ", t.g(Glyph::Connector)), Role::Meta),
            t.span(spinner::brain_frame(t, cx.tick).to_owned(), Role::Accent),
            t.span(" running…".to_owned(), Role::Meta),
        ]));
        return out;
    }
    let (shown, hidden) = collapse(result, cx.collapse, expanded);
    for (i, body) in shown.iter().enumerate() {
        let body_role = if state == ToolState::Failed && i == 0 {
            Role::Err
        } else {
            Role::Meta
        };
        let lead = if i == 0 {
            format!("  {}  ", t.g(Glyph::Connector))
        } else {
            "     ".to_owned()
        };
        out.push(Line::from(vec![
            t.span(lead, Role::Meta),
            t.span(fmt::truncate(body, room), body_role),
        ]));
    }
    if hidden > 0 {
        out.push(Line::from(Span::styled(
            format!("     … +{hidden} lines (ctrl+o to expand)"),
            t.style(Role::Meta).add_modifier(Modifier::DIM),
        )));
    }
    out
}

fn notice(
    glyph: &str,
    role: Role,
    head: &str,
    body: &[String],
    cx: &Ctx<'_>,
) -> Vec<Line<'static>> {
    let t = cx.theme;
    let room = cx.width.saturating_sub(5) as usize;
    let mut out = vec![Line::from(vec![
        t.span(format!("{glyph} "), role),
        t.span(fmt::truncate(head, cx.width.saturating_sub(2) as usize), role),
    ])];
    for (i, line) in body.iter().enumerate() {
        let lead = if i == 0 {
            format!("  {}  ", t.g(Glyph::Connector))
        } else {
            "     ".to_owned()
        };
        out.push(Line::from(vec![
            t.span(lead, Role::Meta),
            t.span(fmt::truncate(line, room), Role::Meta),
        ]));
    }
    out
}

/// A tool result is truncated, never wrapped: a wrapped `cargo test` line is unreadable.
pub fn collapse(body: &[String], limit: usize, expanded: bool) -> (Vec<String>, usize) {
    if expanded || body.len() <= limit {
        return (body.to_vec(), 0);
    }
    (body[..limit].to_vec(), body.len() - limit)
}

fn clip(s: &str, width: u16) -> String {
    fmt::truncate(s, width as usize)
}

/// The text of a block's rendered lines, for `ctrl+o` on a committed block and for tests.
pub fn text_of(lines: &[Line<'_>]) -> Vec<String> {
    lines.iter().map(markdown::text_of).collect()
}

pub mod tool_args {
    use crate::ui::fmt;

    const MCP_PREFIX: &str = "mcp__swamp__";

    pub fn short_name(name: &str) -> String {
        name.strip_prefix(MCP_PREFIX).unwrap_or(name).to_owned()
    }

    /// Per-tool rules where the arguments are structured, a collapsed one-liner otherwise.
    pub fn preview(name: &str, raw: &str, width: u16) -> String {
        let short = short_name(name);
        let room = (width as usize).saturating_sub(short.len() + 6).max(8);
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
            let structured = match short.as_str() {
                "swamp_dispatch" => count(&v, "tasks", "task"),
                "swamp_await" => count(&v, "nodes", "node"),
                "swamp_result" | "swamp_worker_diff" => v
                    .get("node")
                    .and_then(|n| n.as_str())
                    .map(short_node),
                "swamp_status" => Some(String::new()),
                "swamp_note" => v
                    .get("text")
                    .or_else(|| v.get("note"))
                    .and_then(|n| n.as_str())
                    .map(|n| fmt::truncate(n, 40)),
                _ => None,
            };
            if let Some(s) = structured {
                return s;
            }
        }
        fmt::truncate(&collapse_ws(raw), room)
    }

    fn count(v: &serde_json::Value, key: &str, word: &str) -> Option<String> {
        let n = v.get(key)?.as_array()?.len();
        Some(format!("{n} {word}{}", if n == 1 { "" } else { "s" }))
    }

    fn short_node(spec: &str) -> String {
        use std::str::FromStr;
        crate::ids::NodeId::from_str(spec)
            .map(|id| id.short())
            .unwrap_or_else(|_| fmt::truncate(spec, 12))
    }

    fn collapse_ws(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cx(width: u16) -> Ctx<'static> {
        static PLAIN: Theme = Theme {
            palette: crate::ui::chat::theme::Palette::Plain,
            ascii: false,
        };
        Ctx {
            width,
            theme: &PLAIN,
            collapse: 3,
            tick: 0,
            live: false,
        }
    }

    #[test]
    fn a_long_result_collapses_and_ctrl_o_restores_every_line() {
        let body: Vec<String> = (0..17).map(|i| format!("line {i}")).collect();
        let (shown, hidden) = collapse(&body, 3, false);
        assert_eq!(shown.len(), 3);
        assert_eq!(hidden, 14);
        let (all, none) = collapse(&body, 3, true);
        assert_eq!(all.len(), 17);
        assert_eq!(none, 0);

        let block = Block::Tool {
            id: "t1".into(),
            name: "Bash".into(),
            preview: "cargo test".into(),
            state: ToolState::Ok,
            result: body,
            expanded: false,
        };
        let text = text_of(&block.render(&cx(80)));
        assert_eq!(text[0], "● Bash(cargo test)");
        assert!(text.last().unwrap().contains("+14 lines (ctrl+o to expand)"), "{text:?}");
        assert!(block.collapsible(3));
    }

    #[test]
    fn tool_arguments_follow_the_per_tool_rules() {
        assert_eq!(
            tool_args::preview("mcp__swamp__swamp_dispatch", r#"{"tasks":[{},{}]}"#, 100),
            "2 tasks"
        );
        assert_eq!(
            tool_args::preview("mcp__swamp__swamp_status", "{}", 100),
            ""
        );
        assert_eq!(
            tool_args::preview("Bash", "cargo   test\n--all", 100),
            "cargo test --all"
        );
        assert_eq!(tool_args::short_name("mcp__swamp__swamp_await"), "swamp_await");
    }

    #[test]
    fn a_running_tool_shows_a_spinner_and_no_body() {
        let block = Block::Tool {
            id: "t1".into(),
            name: "Read".into(),
            preview: "src/api/users.rs".into(),
            state: ToolState::Running,
            result: Vec::new(),
            expanded: false,
        };
        let text = text_of(&block.render(&cx(80)));
        assert_eq!(text[0], "● Read(src/api/users.rs)");
        assert!(text[1].contains("running…"), "{text:?}");
    }

    #[test]
    fn a_user_bar_is_exactly_one_full_width_row() {
        let block = Block::User("do the thing".into());
        let lines = block.render(&cx(40));
        assert_eq!(lines.len(), 1);
        assert_eq!(text_of(&lines)[0].width(), 40);
    }
}
