//! One key table for every surface's hints, overlays and the README, so keys never drift.

use crate::ui::chat::theme::{Role, Theme};
use crate::ui::order::SEP;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Past this a key cell pushes its help right instead of widening every row.
const KEY_W: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Board,
    Pager,
    Watch,
    Chat,
}

impl Surface {
    pub const ALL: [Surface; 4] = [
        Surface::Board,
        Surface::Pager,
        Surface::Watch,
        Surface::Chat,
    ];

    const fn bit(self) -> u8 {
        match self {
            Surface::Board => 1,
            Surface::Pager => 2,
            Surface::Watch => 4,
            Surface::Chat => 8,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Surface::Board => "board",
            Surface::Pager => "pager",
            Surface::Watch => "watch",
            Surface::Chat => "chat",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Surfaces(u8);

impl Surfaces {
    pub fn has(self, s: Surface) -> bool {
        self.0 & s.bit() != 0
    }
}

const B: u8 = Surface::Board.bit();
const P: u8 = Surface::Pager.bit();
const W: u8 = Surface::Watch.bit();
const C: u8 = Surface::Chat.bit();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Move,
    Cancel,
    Open,
    Raw,
    Fold,
    Stuck,
    Accounts,
    Diff,
    Run,
    Follow,
    AllRuns,
    Ends,
    Scroll,
    Page,
    Confirm,
    Decline,
    Back,
    Keys,
    Quit,
    ClearOrQuit,
    Leave,
    Send,
    Newline,
    Interrupt,
    CancelAll,
    ClearScreen,
    Expand,
    History,
    Commands,
    Edit,
}

#[derive(Debug, Clone, Copy)]
pub struct Binding {
    pub keys: &'static [&'static str],
    pub action: KeyAction,
    pub help: &'static str,
    pub surfaces: Surfaces,
    pub hint: Option<&'static str>,
}

const fn bind(
    keys: &'static [&'static str],
    action: KeyAction,
    help: &'static str,
    surfaces: u8,
    hint: Option<&'static str>,
) -> Binding {
    Binding {
        keys,
        action,
        help,
        surfaces: Surfaces(surfaces),
        hint,
    }
}

use KeyAction as A;

/// Table order is overlay and README order.
pub const BINDINGS: &[Binding] = &[
    bind(
        &["↑", "↓"],
        A::Move,
        "select a row",
        B | W,
        Some("↑↓ select"),
    ),
    bind(
        &["k"],
        A::Cancel,
        "cancel the selected task or dispatch (asks y / n)",
        B | W,
        Some("k cancel"),
    ),
    bind(
        &["enter"],
        A::Open,
        "open the selected row: trace, dispatch or accounts",
        B,
        Some("enter open"),
    ),
    bind(
        &["r"],
        A::Raw,
        "raw view: the run journal (board), the node stream (watch)",
        B | P | W,
        Some("r raw"),
    ),
    bind(
        &["←", "→"],
        A::Fold,
        "fold / unfold the selected dispatch",
        B,
        Some("←→ fold"),
    ),
    bind(
        &["!"],
        A::Stuck,
        "jump to the next stuck row",
        B,
        Some("! stuck"),
    ),
    bind(
        &["a"],
        A::Accounts,
        "accounts view",
        B | W,
        Some("a accounts"),
    ),
    bind(
        &["d"],
        A::Diff,
        "the selected node's patch in $PAGER",
        W,
        Some("d diff"),
    ),
    bind(
        &["tab", "shift+tab"],
        A::Run,
        "next / previous run",
        B,
        Some("tab run"),
    ),
    bind(
        &["f"],
        A::Follow,
        "follow the newest running task",
        B,
        Some("f follow"),
    ),
    bind(&["0"], A::AllRuns, "all runs merged", B, Some("0 all runs")),
    bind(
        &["g", "G", "home", "end"],
        A::Ends,
        "top / bottom",
        B | P,
        Some("g G ends"),
    ),
    bind(&["↑", "↓"], A::Scroll, "scroll", P, Some("↑↓ scroll")),
    bind(&["pgup", "pgdn"], A::Page, "scroll a page", P, None),
    bind(&["y"], A::Confirm, "confirm the prompt", B | W, None),
    bind(&["n"], A::Decline, "dismiss the prompt", B | W, None),
    bind(
        &["esc"],
        A::Back,
        "close the pager, overlay or prompt",
        B | P | W,
        Some("esc back"),
    ),
    bind(
        &["?"],
        A::Keys,
        "key list (chat: on an empty input)",
        B | W | C,
        Some("? keys"),
    ),
    bind(&["q"], A::Quit, "quit", B | P | W, Some("q quit")),
    bind(
        &["ctrl+c"],
        A::ClearOrQuit,
        "quit (chat: clear the input, again to leave)",
        B | P | W | C,
        None,
    ),
    bind(&["ctrl+d"], A::Leave, "quit", B | P | W | C, None),
    bind(
        &["enter"],
        A::Send,
        "send; with the popup open, complete the command",
        C,
        None,
    ),
    bind(
        &["alt+enter", "shift+enter", "\\ enter"],
        A::Newline,
        "newline (shift+enter needs the kitty keyboard protocol)",
        C,
        None,
    ),
    bind(
        &["esc"],
        A::Interrupt,
        "close the popup, else interrupt the turn",
        C,
        None,
    ),
    bind(
        &["esc esc"],
        A::CancelAll,
        "cancel every running worker, within 2 s of the first esc",
        C,
        None,
    ),
    bind(
        &["ctrl+l"],
        A::ClearScreen,
        "clear the screen; the scrollback above is untouched",
        C,
        None,
    ),
    bind(
        &["ctrl+o"],
        A::Expand,
        "expand the last collapsed result or dispatch block",
        C,
        None,
    ),
    bind(
        &["↑", "↓"],
        A::History,
        "history on an empty input, else move between lines",
        C,
        None,
    ),
    bind(
        &["/"],
        A::Commands,
        "command popup; tab completes, ↑↓ chooses",
        C,
        None,
    ),
    bind(
        &[
            "ctrl+a", "ctrl+e", "ctrl+k", "ctrl+u", "ctrl+w", "alt+←", "alt+→",
        ],
        A::Edit,
        "readline editing",
        C,
        None,
    ),
];

pub const BOARD_HINTS: &[KeyAction] = &[
    A::Move,
    A::Cancel,
    A::Open,
    A::Raw,
    A::Fold,
    A::Stuck,
    A::Accounts,
    A::Run,
    A::Follow,
    A::AllRuns,
    A::Ends,
];
pub const WATCH_HINTS: &[KeyAction] = &[A::Move, A::Cancel, A::Raw, A::Diff, A::Accounts];
pub const PAGER_HINTS: &[KeyAction] = &[A::Scroll, A::Raw, A::Ends];

/// The hint line of a surface: its head, as much of its ranked list as fits, its tail.
pub fn lists(
    s: Surface,
) -> (
    &'static [KeyAction],
    &'static [KeyAction],
    &'static [KeyAction],
) {
    match s {
        Surface::Board => (&[], BOARD_HINTS, &[A::Keys, A::Quit]),
        Surface::Watch => (&[], WATCH_HINTS, &[A::Keys, A::Quit]),
        Surface::Pager => (&[A::Back], PAGER_HINTS, &[A::Quit]),
        Surface::Chat => (&[], &[], &[]),
    }
}

pub fn binding(a: KeyAction) -> Option<&'static Binding> {
    BINDINGS.iter().find(|b| b.action == a)
}

/// The spelling a key event has in the table, or `None` for a key nothing binds.
pub fn name(k: &KeyEvent) -> Option<String> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    let base = match k.code {
        KeyCode::Char(c) if ctrl => return Some(format!("ctrl+{}", c.to_ascii_lowercase())),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Up => "↑".to_owned(),
        KeyCode::Down => "↓".to_owned(),
        KeyCode::Left => "←".to_owned(),
        KeyCode::Right => "→".to_owned(),
        KeyCode::Enter => "enter".to_owned(),
        KeyCode::Esc => "esc".to_owned(),
        KeyCode::Tab => "tab".to_owned(),
        KeyCode::BackTab => return Some("shift+tab".to_owned()),
        KeyCode::PageUp => "pgup".to_owned(),
        KeyCode::PageDown => "pgdn".to_owned(),
        KeyCode::Home => "home".to_owned(),
        KeyCode::End => "end".to_owned(),
        _ => return None,
    };
    let named = !matches!(k.code, KeyCode::Char(_));
    Some(match (alt, shift && named) {
        (true, _) => format!("alt+{base}"),
        (false, true) => format!("shift+{base}"),
        (false, false) => base,
    })
}

/// What a key does on a surface, straight from the table.
pub fn action(s: Surface, k: &KeyEvent) -> Option<KeyAction> {
    let name = name(k)?;
    BINDINGS
        .iter()
        .find(|b| b.surfaces.has(s) && b.keys.contains(&name.as_str()))
        .map(|b| b.action)
}

fn label(a: KeyAction) -> Option<&'static str> {
    binding(a).and_then(|b| b.hint)
}

pub fn hints(s: Surface, width: usize, hide: &[KeyAction]) -> String {
    let (head, list, tail) = lists(s);
    let shown = |l: &[KeyAction]| -> Vec<&'static str> {
        l.iter()
            .filter(|a| !hide.contains(a))
            .filter_map(|a| label(*a))
            .collect()
    };
    let (head, tail) = (shown(head), shown(tail));
    let mut chosen: Vec<&str> = Vec::new();
    for l in shown(list) {
        let mut all = head.clone();
        all.extend(&chosen);
        all.push(l);
        all.extend(&tail);
        if all.join(SEP).width() > width {
            break;
        }
        chosen.push(l);
    }
    let mut all = head;
    all.extend(chosen);
    all.extend(tail);
    all.join(SEP)
}

/// Every binding a surface lists, as `(keys, help)`.
pub fn entries(s: Surface, hide: &[KeyAction]) -> Vec<(String, &'static str)> {
    BINDINGS
        .iter()
        .filter(|b| b.surfaces.has(s) && !hide.contains(&b.action))
        .map(|b| (b.keys.join(" "), b.help))
        .collect()
}

/// Padded `(key, help)` cells, two pairs per line where they fit `width`.
pub fn overlay_rows(s: Surface, width: usize, hide: &[KeyAction]) -> Vec<Vec<(String, String)>> {
    let entries = entries(s, hide);
    let key_w = |col: &[&(String, &str)]| {
        col.iter()
            .map(|e| e.0.width())
            .max()
            .unwrap_or(0)
            .min(KEY_W)
    };
    let left: Vec<&(String, &str)> = entries.iter().step_by(2).collect();
    let right: Vec<&(String, &str)> = entries.iter().skip(1).step_by(2).collect();
    let (kl, kr) = (key_w(&left), key_w(&right));
    let hl = left.iter().map(|e| e.1.width()).max().unwrap_or(0);
    let cell = |e: &(String, &str), kw: usize, hw: Option<usize>| {
        let key = format!("{:<kw$}  ", e.0);
        let help = match hw {
            Some(hw) => format!("{:<hw$}  ", e.1),
            None => e.1.to_owned(),
        };
        (key, help)
    };
    let fits = entries.chunks(2).all(|pair| match pair {
        [a, b] => {
            2 + a.0.width().max(kl) + 2 + hl + 2 + b.0.width().max(kr) + 2 + b.1.width() <= width
        }
        _ => true,
    });
    if !fits {
        let kw = key_w(&entries.iter().collect::<Vec<_>>());
        return entries.iter().map(|e| vec![cell(e, kw, None)]).collect();
    }
    entries
        .chunks(2)
        .map(|pair| match pair {
            [a, b] => vec![cell(a, kl, Some(hl)), cell(b, kr, None)],
            [a] => vec![cell(a, kl, None)],
            _ => Vec::new(),
        })
        .collect()
}

pub fn overlay(s: Surface, t: &Theme, width: usize, hide: &[KeyAction]) -> Vec<Line<'static>> {
    overlay_rows(s, width, hide)
        .into_iter()
        .map(|row| {
            let mut spans = vec![Span::raw("  ")];
            for (k, h) in row {
                spans.push(t.span(k, Role::Name));
                spans.push(t.span(h, Role::Meta));
            }
            Line::from(spans)
        })
        .collect()
}

/// The same rows as plain text, for surfaces that style their own lines.
pub fn overlay_text(s: Surface, width: usize, hide: &[KeyAction]) -> Vec<String> {
    overlay_rows(s, width, hide)
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|(k, h)| k + &h)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

/// The README `## Keys` table, between its markers.
pub fn readme_table() -> String {
    let mut out = String::from("| Key | What it does | Board | Pager | Watch | Chat |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    for b in BINDINGS {
        let keys: Vec<String> = b.keys.iter().map(|k| format!("`{k}`")).collect();
        let marks: Vec<&str> = Surface::ALL
            .iter()
            .map(|s| if b.surfaces.has(*s) { "✓" } else { "" })
            .collect();
        out.push_str(&format!(
            "| {} | {} | {} |\n",
            keys.join(" "),
            b.help,
            marks.join(" | ")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::chat::blocks::text_of;

    const BEGIN: &str = "<!-- keys:begin -->\n";
    const END: &str = "<!-- keys:end -->";

    fn readme() -> String {
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"))
            .expect("README.md")
    }

    #[test]
    fn the_readme_table_is_the_key_table() {
        let text = readme();
        let start = text.find(BEGIN).expect("README keys:begin marker") + BEGIN.len();
        let end = text.find(END).expect("README keys:end marker");
        let want = readme_table();
        assert!(
            text[start..end].trim_end() == want.trim_end(),
            "README.md drifted from ui::keys; paste this between the markers:\n\n{want}"
        );
    }

    #[test]
    fn every_overlay_lists_every_binding_of_its_surface() {
        for s in Surface::ALL {
            let lines = text_of(&overlay(s, &Theme::plain(), 1000, &[])).join("\n");
            let narrow = overlay_text(s, 40, &[]).join("\n");
            for b in BINDINGS.iter().filter(|b| b.surfaces.has(s)) {
                for text in [&lines, &narrow] {
                    assert!(text.contains(&b.keys.join(" ")), "{s:?}: {:?}", b.keys);
                    assert!(text.contains(b.help), "{s:?}: {}", b.help);
                }
            }
        }
    }

    #[test]
    fn every_hint_list_names_bindings_its_surface_has() {
        for s in Surface::ALL {
            let line = hints(s, 1000, &[]);
            let (head, list, tail) = lists(s);
            for a in head.iter().chain(list).chain(tail) {
                let b = binding(*a).expect("a binding per hinted action");
                assert!(b.surfaces.has(s), "{s:?} hints {a:?} but does not bind it");
                let label = b.hint.expect("a hinted action has a label");
                assert!(line.contains(label), "{s:?}: {label} missing from {line}");
            }
        }
    }

    #[test]
    fn no_key_is_bound_twice_on_one_surface() {
        for s in Surface::ALL {
            let mut seen: Vec<&str> = Vec::new();
            for b in BINDINGS.iter().filter(|b| b.surfaces.has(s)) {
                for k in b.keys {
                    assert!(!seen.contains(k), "{s:?}: `{k}` is bound twice");
                    seen.push(k);
                }
            }
        }
    }

    #[test]
    fn no_action_has_two_bindings() {
        for (i, b) in BINDINGS.iter().enumerate() {
            assert!(
                BINDINGS[i + 1..].iter().all(|o| o.action != b.action),
                "{:?} is bound twice",
                b.action
            );
        }
    }

    #[test]
    fn cancel_is_the_same_key_on_the_board_and_in_watch() {
        let k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(action(Surface::Board, &k), Some(KeyAction::Cancel));
        assert_eq!(action(Surface::Watch, &k), Some(KeyAction::Cancel));
        assert_eq!(action(Surface::Pager, &k), None);
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(action(Surface::Pager, &up), Some(KeyAction::Scroll));
        let back = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(action(Surface::Board, &back), Some(KeyAction::Run));
        let big_g = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(action(Surface::Board, &big_g), Some(KeyAction::Ends));
        for code in [KeyCode::Home, KeyCode::End] {
            let k = KeyEvent::new(code, KeyModifiers::NONE);
            assert_eq!(action(Surface::Pager, &k), Some(KeyAction::Ends));
        }
    }

    #[test]
    fn hints_keep_their_head_and_tail_and_fill_in_order() {
        assert_eq!(
            hints(Surface::Board, 40, &[]),
            "↑↓ select · k cancel · ? keys · q quit"
        );
        assert_eq!(
            hints(Surface::Board, 60, &[]),
            "↑↓ select · k cancel · enter open · r raw · ? keys · q quit"
        );
        assert_eq!(
            hints(Surface::Pager, 80, &[KeyAction::Raw]),
            "esc back · ↑↓ scroll · g G ends · q quit"
        );
        assert!(!hints(Surface::Board, 200, &[KeyAction::Cancel]).contains("cancel"));
    }
}
