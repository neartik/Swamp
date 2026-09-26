//! Task ranking, per-state tallies and the cell fitting shared by the board and chat.

use crate::journal::inspect::AttemptDetail;
use crate::model::core::NodeState;
use crate::ui::chat::theme::{Role, Theme};
use crate::ui::{fmt, trace};
use ratatui::text::Span;
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

pub const SEP: &str = " \u{b7} ";

/// 0 stuck for good, 1 running, 2 blocked, 3 queued, 4 finished.
pub fn rank(s: &NodeState) -> u8 {
    match s {
        NodeState::Failed { .. } | NodeState::Rejected { .. } | NodeState::Orphaned { .. } => 0,
        NodeState::Running { .. } | NodeState::Leased { .. } => 1,
        NodeState::Blocked { .. } => 2,
        NodeState::Queued => 3,
        NodeState::Succeeded | NodeState::Cancelled { .. } => 4,
    }
}

/// Tasks by state, in rank order. Running counts leased tasks too.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    pub failed: usize,
    pub rejected: usize,
    pub orphaned: usize,
    pub running: usize,
    pub blocked: usize,
    pub queued: usize,
    pub done: usize,
    pub cancelled: usize,
}

impl Tally {
    pub fn add(&mut self, s: &NodeState) {
        let n = match s {
            NodeState::Failed { .. } => &mut self.failed,
            NodeState::Rejected { .. } => &mut self.rejected,
            NodeState::Orphaned { .. } => &mut self.orphaned,
            NodeState::Running { .. } | NodeState::Leased { .. } => &mut self.running,
            NodeState::Blocked { .. } => &mut self.blocked,
            NodeState::Queued => &mut self.queued,
            NodeState::Succeeded => &mut self.done,
            NodeState::Cancelled { .. } => &mut self.cancelled,
        };
        *n += 1;
    }

    pub fn absorb(&mut self, o: &Tally) {
        self.failed += o.failed;
        self.rejected += o.rejected;
        self.orphaned += o.orphaned;
        self.running += o.running;
        self.blocked += o.blocked;
        self.queued += o.queued;
        self.done += o.done;
        self.cancelled += o.cancelled;
    }

    pub fn stuck(&self) -> usize {
        self.failed + self.rejected + self.orphaned + self.blocked
    }

    /// Tasks not in a terminal state.
    pub fn live(&self) -> usize {
        self.orphaned + self.running + self.blocked + self.queued
    }

    pub fn total(&self) -> usize {
        self.live() + self.failed + self.rejected + self.done + self.cancelled
    }

    /// The non-zero counts, in rank order.
    pub fn cells(&self) -> Vec<Cell> {
        [
            ("failed", self.failed, Role::Err),
            ("rejected", self.rejected, Role::Err),
            ("orphaned", self.orphaned, Role::Err),
            ("running", self.running, Role::Meta),
            ("blocked", self.blocked, Role::Err),
            ("queued", self.queued, Role::Meta),
            ("done", self.done, Role::Meta),
            ("cancelled", self.cancelled, Role::Meta),
        ]
        .into_iter()
        .filter(|(_, n, _)| *n > 0)
        .map(|(key, n, role)| Cell::new(key, format!("{n} {key}"), role))
        .collect()
    }
}

const COUNTS: [&str; 8] = [
    "failed",
    "rejected",
    "orphaned",
    "running",
    "blocked",
    "queued",
    "done",
    "cancelled",
];

/// One cell of a ` · ` separated line. `glue` is what goes before it when it is not first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub key: &'static str,
    pub text: String,
    pub role: Role,
    pub glue: &'static str,
}

impl Cell {
    pub fn new(key: &'static str, text: impl Into<String>, role: Role) -> Cell {
        Cell {
            key,
            text: text.into(),
            role,
            glue: SEP,
        }
    }

    /// Attached to the previous cell by a single space rather than ` · `.
    pub fn joined(mut self) -> Cell {
        self.glue = " ";
        self
    }
}

/// What `fit` may drop, in the order given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drop {
    /// The last cell with this key.
    Key(&'static str),
    /// Trailing queued, done and cancelled counts, last first, stopping at any other count.
    Tail,
}

pub fn width(cells: &[Cell]) -> usize {
    cells
        .iter()
        .enumerate()
        .map(|(i, c)| c.text.width() + if i == 0 { 0 } else { c.glue.width() })
        .sum()
}

/// Drops cells in `order` until the line fits `room`; what still does not fit is truncated
/// when it is drawn.
pub fn fit(mut cells: Vec<Cell>, room: usize, order: &[Drop]) -> Vec<Cell> {
    for d in order {
        if width(&cells) <= room {
            break;
        }
        match d {
            Drop::Key(key) => {
                if let Some(i) = cells.iter().rposition(|c| c.key == *key) {
                    cells.remove(i);
                }
            }
            Drop::Tail => {
                while width(&cells) > room {
                    let Some(i) = cells.iter().rposition(|c| COUNTS.contains(&c.key)) else {
                        break;
                    };
                    if !matches!(cells[i].key, "queued" | "done" | "cancelled") {
                        break;
                    }
                    cells.remove(i);
                }
            }
        }
    }
    cells
}

pub fn text(cells: &[Cell]) -> String {
    let mut out = String::new();
    for (i, c) in cells.iter().enumerate() {
        if i > 0 {
            out.push_str(c.glue);
        }
        out.push_str(&c.text);
    }
    out
}

/// The cells as styled spans, cut with `…` at `room`.
pub fn spans(cells: &[Cell], t: &Theme, room: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for (i, c) in cells.iter().enumerate() {
        let parts = if i == 0 {
            [None, Some((c.text.as_str(), c.role))]
        } else {
            [Some((c.glue, Role::Meta)), Some((c.text.as_str(), c.role))]
        };
        for (s, role) in parts.into_iter().flatten() {
            if used >= room {
                return out;
            }
            let cut = fmt::truncate(s, room - used);
            used += cut.width();
            out.push(t.span(cut, role));
        }
    }
    out
}

/// `attempt 1 9g5f08 on main: rate_limited (five_hour) after 41s`, one per earlier attempt,
/// or one summary line once there are more than two.
pub fn attempt_lines(prior: &[AttemptDetail]) -> Vec<String> {
    let line = |a: &AttemptDetail| {
        let why = match &a.failure {
            Some(f) => trace::failure_short(f),
            None => fmt::phase_word(a.state).to_owned(),
        };
        let after = a
            .elapsed_ms
            .map(|ms| format!(" after {}", fmt::duration(Duration::from_millis(ms))))
            .unwrap_or_default();
        format!(
            "attempt {} {} on {}: {why}{after}",
            a.attempt,
            a.node.short(),
            a.account.as_ref().map_or("-", |id| id.0.as_str())
        )
    };
    match prior {
        [] => Vec::new(),
        [_] | [_, _] => prior.iter().map(line).collect(),
        [.., last] => vec![format!(
            "{} prior attempts{SEP}last: {}",
            prior.len(),
            line(last)
        )],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::core::{AccountId, CancelSource};
    use crate::model::failure::Failure;

    fn cells(keys: &[(&'static str, &str)]) -> Vec<Cell> {
        keys.iter()
            .map(|(k, t)| Cell::new(k, *t, Role::Meta))
            .collect()
    }

    #[test]
    fn failures_rank_first_then_running_blocked_queued_done() {
        let failed = NodeState::Failed {
            failure: Failure::Timeout { after_s: 1 },
        };
        let running = NodeState::Leased {
            account: AccountId("main".into()),
        };
        let blocked = NodeState::Blocked {
            until: time::OffsetDateTime::UNIX_EPOCH,
            why: String::new(),
        };
        let done = NodeState::Cancelled {
            by: CancelSource::User,
        };
        let ranks: Vec<u8> = [&failed, &running, &blocked, &NodeState::Queued, &done]
            .iter()
            .map(|s| rank(s))
            .collect();
        assert_eq!(ranks, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn a_tally_names_only_what_it_saw_in_rank_order() {
        let mut t = Tally::default();
        t.add(&NodeState::Succeeded);
        t.add(&NodeState::Queued);
        t.add(&NodeState::Blocked {
            until: time::OffsetDateTime::UNIX_EPOCH,
            why: String::new(),
        });
        assert_eq!(text(&t.cells()), "1 blocked · 1 queued · 1 done");
        assert_eq!((t.stuck(), t.live(), t.total()), (1, 2, 3));
    }

    #[test]
    fn fit_drops_the_tail_counts_but_never_a_failure() {
        let line = cells(&[
            ("label", "#1"),
            ("tasks", "5 tasks"),
            ("failed", "1 failed"),
            ("queued", "1 queued"),
            ("done", "1 done"),
        ]);
        let order = [Drop::Key("tasks"), Drop::Tail];
        assert_eq!(text(&fit(line.clone(), 100, &order)), text(&line));
        assert_eq!(
            text(&fit(line.clone(), 24, &order)),
            "#1 · 1 failed · 1 queued"
        );
        assert_eq!(text(&fit(line, 5, &order)), "#1 · 1 failed");
    }

    #[test]
    fn a_joined_cell_takes_a_space_not_a_dot() {
        let line = vec![
            Cell::new("label", "#1", Role::Name),
            Cell::new("id", "9g5f18", Role::Meta).joined(),
            Cell::new("running", "2 running", Role::Meta),
        ];
        assert_eq!(text(&line), "#1 9g5f18 · 2 running");
        assert_eq!(width(&line), 21);
        assert_eq!(text(&fit(line, 14, &[Drop::Key("id")])), "#1 · 2 running");
    }
}
