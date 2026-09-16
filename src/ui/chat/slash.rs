use crate::ui::chat::theme::{Glyph, Role, Theme};
use crate::ui::fmt;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

pub struct Cmd {
    pub name: &'static str,
    pub args: &'static str,
    pub help: &'static str,
}

/// One table feeds the popup, `/help` and completion, so the three cannot drift apart.
pub const COMMANDS: &[Cmd] = &[
    Cmd {
        name: "/help",
        args: "",
        help: "commands and keyboard shortcuts",
    },
    Cmd {
        name: "/status",
        args: "",
        help: "the run tree, same output as swamp trace",
    },
    Cmd {
        name: "/accounts",
        args: "",
        help: "the account pool, health and spend",
    },
    Cmd {
        name: "/usage",
        args: "[--json]",
        help: "per-account tokens and quota windows",
    },
    Cmd {
        name: "/trace",
        args: "[node]",
        help: "the run tree with its events",
    },
    Cmd {
        name: "/cost",
        args: "",
        help: "tokens and spend for this run",
    },
    Cmd {
        name: "/tier",
        args: "[low|mid|high]",
        help: "show or set the default dispatch tier",
    },
    Cmd {
        name: "/cancel",
        args: "<node|all>",
        help: "stop one worker or all of them",
    },
    Cmd {
        name: "/diff",
        args: "<node>",
        help: "the stat of a worker's captured patch",
    },
    Cmd {
        name: "/thinking",
        args: "[on|off]",
        help: "show or hide the brain's thinking",
    },
    Cmd {
        name: "/clear",
        args: "",
        help: "clear the screen, keep the conversation",
    },
    Cmd {
        name: "/resume",
        args: "<run|last>",
        help: "the command that reopens a run",
    },
    Cmd {
        name: "/quit",
        args: "",
        help: "leave the chat",
    },
];

pub const MAX_ROWS: usize = 8;
const NAME_WIDTH: usize = 12;

/// The shortcut overlay, and the tail of `/help`.
pub const SHORTCUTS: [[&str; 4]; 5] = [
    ["enter", "send", "ctrl+o", "expand the last result"],
    [
        "alt+enter",
        "newline (also shift+enter)",
        "ctrl+l",
        "clear the screen",
    ],
    [
        "esc",
        "interrupt the turn",
        "ctrl+c",
        "clear input, twice to quit",
    ],
    ["esc esc", "cancel running workers", "ctrl+d", "quit"],
    ["↑ ↓", "history (empty input)", "/", "commands"],
];

/// Prefix matches first, then substring matches on name and description, stably.
pub fn filter(input: &str) -> Vec<&'static Cmd> {
    let needle = input.trim_start_matches('/').to_ascii_lowercase();
    if needle.is_empty() {
        return COMMANDS.iter().collect();
    }
    let mut prefix = Vec::new();
    let mut rest = Vec::new();
    for c in COMMANDS {
        let name = c.name.trim_start_matches('/').to_ascii_lowercase();
        if name.starts_with(&needle) {
            prefix.push(c);
        } else if name.contains(&needle) || c.help.to_ascii_lowercase().contains(&needle) {
            rest.push(c);
        }
    }
    prefix.extend(rest);
    prefix
}

/// What `tab` types for you: the longest prefix every name-prefix candidate shares. A
/// description match is a hint, never something completion is allowed to type.
pub fn common_prefix(input: &str) -> String {
    let typed = input.trim().to_ascii_lowercase();
    let names: Vec<&str> = COMMANDS
        .iter()
        .map(|c| c.name)
        .filter(|n| n.to_ascii_lowercase().starts_with(&typed))
        .collect();
    let Some(first) = names.first() else {
        return typed;
    };
    let mut prefix = (*first).to_owned();
    for name in names.iter().skip(1) {
        while !name.starts_with(&prefix) {
            prefix.pop();
            if prefix.is_empty() {
                return prefix;
            }
        }
    }
    prefix
}

/// Whether enter should run the line instead of completing it: the typed text already names a
/// command, arguments and all, or leaves one candidate spelled exactly as typed.
pub fn runnable(input: &str) -> bool {
    let typed = input.trim();
    let head = typed.split_whitespace().next().unwrap_or_default();
    if find(head).is_some() {
        return true;
    }
    let cands = filter(typed);
    cands.len() == 1 && cands[0].name == typed
}

pub fn find(name: &str) -> Option<&'static Cmd> {
    let name = name.trim();
    COMMANDS.iter().find(|c| c.name == name)
}

/// Commands removed outright, pointing at what replaced them: too far apart in spelling for
/// `distance` to ever suggest the right one.
const REPLACED: &[(&str, &str)] = &[("workers", "/usage")];

/// `unknown command /foo; did you mean /force?` beats a bare refusal.
pub fn did_you_mean(name: &str) -> String {
    let bare = name.trim_start_matches('/');
    if let Some((_, to)) = REPLACED.iter().find(|(from, _)| *from == bare) {
        return format!("did you mean {to}?");
    }
    let best = COMMANDS
        .iter()
        .map(|c| (distance(bare, c.name.trim_start_matches('/')), c.name))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d);
    match best {
        Some((_, name)) => format!("did you mean {name}?"),
        None => "try /help".to_owned(),
    }
}

fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let next = (row[j + 1] + 1).min(row[j] + 1).min(prev + cost);
            prev = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// Sits directly above the top rule, no border, the matched prefix in `accent`.
pub fn popup(input: &str, selected: usize, width: u16, t: &Theme) -> Vec<Line<'static>> {
    let cands = filter(input);
    let needle = input.trim_start_matches('/').len();
    let mut out = Vec::new();
    for (i, c) in cands.iter().take(MAX_ROWS).enumerate() {
        let picked = i == selected;
        let mark = if picked {
            t.span(t.g(Glyph::Select).to_owned(), Role::Accent)
        } else {
            Span::raw(" ")
        };
        let (matched, rest) = c.name.split_at((needle + 1).min(c.name.len()));
        let name_width = NAME_WIDTH.saturating_sub(matched.width());
        let mut spans = vec![
            mark,
            Span::raw(" "),
            t.span(matched.to_owned(), Role::Accent),
            t.span(format!("{rest:<name_width$}"), Role::Name),
            t.span(
                fmt::truncate(c.help, width.saturating_sub(16) as usize),
                Role::Meta,
            ),
        ];
        if picked {
            let used: usize = spans.iter().map(|s| s.content.width()).sum();
            spans.push(Span::raw(" ".repeat((width as usize).saturating_sub(used))));
            for s in &mut spans {
                s.style = s.style.add_modifier(Modifier::REVERSED);
            }
        }
        out.push(Line::from(spans));
    }
    if cands.len() > MAX_ROWS {
        out.push(Line::from(t.span(
            format!("  … +{} more", cands.len() - MAX_ROWS),
            Role::Meta,
        )));
    }
    out
}

pub fn overlay(t: &Theme) -> Vec<Line<'static>> {
    SHORTCUTS
        .iter()
        .map(|[key, what, key2, what2]| {
            Line::from(vec![
                Span::raw("  "),
                t.span(format!("{key:<14}"), Role::Name),
                t.span(format!("{what:<31}"), Role::Meta),
                t.span(format!("{key2:<10}"), Role::Name),
                t.span((*what2).to_owned(), Role::Meta),
            ])
        })
        .collect()
}

/// `/help`: the command table plus the shortcut block, as one committed block body.
pub fn help_body() -> Vec<String> {
    let mut out: Vec<String> = COMMANDS
        .iter()
        .map(|c| {
            let spelling = if c.args.is_empty() {
                c.name.to_owned()
            } else {
                format!("{} {}", c.name, c.args)
            };
            format!("{spelling:<26}{}", c.help)
        })
        .collect();
    out.push(String::new());
    for [key, what, key2, what2] in SHORTCUTS {
        out.push(format!("{key:<14}{what:<31}{key2:<10}{what2}"));
    }
    out.push(String::new());
    out.push("shift+enter needs a terminal that speaks the kitty keyboard protocol;".to_owned());
    out.push("alt+enter and a trailing backslash always work.".to_owned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UI.md illustrated the refused-input status line with `/adopt`, which has never been a
    /// slash command: adopting is CLI-only. Every command an example refuses must exist.
    #[test]
    fn the_ui_doc_refusal_examples_name_real_commands() {
        let doc = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/UI.md"),
        )
        .expect("UI.md");
        let mut seen = 0;
        for line in doc.lines().filter(|l| l.contains("try /")) {
            for word in line.split_whitespace() {
                let name = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '/');
                if !name.starts_with('/') {
                    continue;
                }
                seen += 1;
                assert!(
                    COMMANDS.iter().any(|c| c.name == name),
                    "UI.md names `{name}`, which is not a slash command: {line}"
                );
            }
        }
        assert!(seen > 0, "no refusal example left to check");
    }

    #[test]
    fn tab_completes_to_the_common_prefix_of_every_candidate() {
        let cands = filter("/t");
        let names: Vec<&str> = cands.iter().map(|c| c.name).collect();
        assert!(names.starts_with(&["/trace", "/tier", "/thinking"]) || names.len() >= 3);
        assert_eq!(common_prefix("/tr"), "/trace");
        assert_eq!(common_prefix("/t"), "/t");
        assert_eq!(
            common_prefix("/c"),
            "/c",
            "/cost, /cancel, /clear share only /c"
        );
    }

    #[test]
    fn enter_runs_a_name_that_is_already_typed_out_and_completes_anything_else() {
        assert!(runnable("/status"));
        assert!(runnable("/tier low"), "arguments do not make it ambiguous");
        assert!(!runnable("/t"), "three candidates, so enter completes");
        assert!(!runnable("/q"), "one candidate, but not spelled out");
        assert!(!runnable("/nope"));
    }

    #[test]
    fn a_typo_is_matched_to_the_nearest_command() {
        assert_eq!(did_you_mean("/statu"), "did you mean /status?");
        assert_eq!(did_you_mean("/zzzzzzz"), "try /help");
    }

    /// `/workers` was removed outright; `/usage` replaces it in spirit, not in spelling.
    #[test]
    fn workers_is_gone_and_points_at_usage() {
        assert!(find("/workers").is_none());
        assert_eq!(did_you_mean("/workers"), "did you mean /usage?");
    }

    #[test]
    fn the_popup_marks_the_selection_and_caps_at_eight_rows() {
        let lines = popup("/", 1, 100, &Theme::plain());
        assert_eq!(lines.len(), MAX_ROWS + 1);
        let text = crate::ui::chat::blocks::text_of(&lines);
        assert!(text[1].starts_with('▌'), "{text:?}");
        let hidden = COMMANDS.len() - MAX_ROWS;
        assert!(
            text.last().unwrap().contains(&format!("+{hidden} more")),
            "{text:?}"
        );
    }
}
