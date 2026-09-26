//! The delegation metric, `RunView::brain_self_work`, as trace, dispatches and the board print it.

use crate::journal::fold::{BrainSelfWork, RunView};
use crate::ui::chat::theme::Role;
use crate::ui::order::Cell;
use serde::Serialize;

/// The `--json` shape: `swamp dispatches`, `swamp trace` and `swamp board` all carry it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SelfWork {
    pub calls: u32,
    pub budget: u32,
    pub over_budget: bool,
    pub dispatched: bool,
    pub brain_usd: Option<f64>,
    pub total_usd: f64,
    pub cost_share: Option<f64>,
    /// False when some node reported no cost: `total_usd` is a floor, `cost_share` a ceiling.
    pub cost_complete: bool,
}

pub fn json(view: &RunView, budget: u32) -> Option<SelfWork> {
    let w = view.brain_self_work()?;
    Some(SelfWork {
        calls: w.calls,
        budget,
        over_budget: w.over(budget),
        dispatched: w.dispatched,
        brain_usd: w.brain_usd.map(|usd| round(usd, 6)),
        total_usd: round(w.total_usd, 6),
        cost_share: w.cost_share().map(|share| round(share, 4)),
        cost_complete: w.cost_complete,
    })
}

/// Sums of floats carry noise (`1.9000000000000001`) that JSON consumers would diff on.
fn round(x: f64, places: i32) -> f64 {
    let scale = 10f64.powi(places);
    (x * scale).round() / scale
}

/// `brain  3/8 calls before the first dispatch, 12% of the cost`, flagged past the budget.
pub fn line(w: &BrainSelfWork, budget: u32) -> String {
    let when = if w.dispatched {
        "before the first dispatch"
    } else {
        "and no dispatch yet"
    };
    let mut out = format!("brain  {}/{budget} calls {when}", w.calls);
    if let Some(share) = w.cost_share() {
        let known = if w.cost_complete { "" } else { "known " };
        out.push_str(&format!(", {} of the {known}cost", percent(share)));
    }
    if w.over(budget) {
        out.push_str(": over limits.brain_read_budget");
    }
    out.push('\n');
    out
}

/// `brain 3/8 (12%)` for the board header, red and marked `over` past the budget.
pub fn cell(w: &BrainSelfWork, budget: u32, share: bool) -> Cell {
    let over = w.over(budget);
    let mut text = format!("brain {}/{budget}", w.calls);
    if over {
        text.push_str(" over");
    }
    if let Some(part) = w.cost_share().filter(|_| share) {
        let tilde = if w.cost_complete { "" } else { "~" };
        text.push_str(&format!(" ({tilde}{})", percent(part)));
    }
    Cell::new("brain", text, if over { Role::Err } else { Role::Meta })
}

fn percent(share: f64) -> String {
    format!("{:.0}%", share * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(calls: u32, dispatched: bool, brain_usd: Option<f64>) -> BrainSelfWork {
        BrainSelfWork {
            calls,
            dispatched,
            brain_usd,
            total_usd: 4.0,
            cost_complete: true,
        }
    }

    #[test]
    fn the_line_names_the_budget_and_the_share() {
        assert_eq!(
            line(&work(3, true, Some(0.5)), 8),
            "brain  3/8 calls before the first dispatch, 12% of the cost\n"
        );
        assert_eq!(
            line(&work(5, false, None), 8),
            "brain  5/8 calls and no dispatch yet\n"
        );
    }

    #[test]
    fn past_the_budget_the_line_and_the_cell_warn() {
        let w = work(12, true, Some(3.0));
        assert!(line(&w, 8).ends_with(", 75% of the cost: over limits.brain_read_budget\n"));
        let c = cell(&w, 8, true);
        assert_eq!(c.text, "brain 12/8 over (75%)");
        assert_eq!(c.role, Role::Err);
        assert_eq!(cell(&w, 8, false).text, "brain 12/8 over");
        let c = cell(&work(8, true, None), 8, true);
        assert_eq!((c.text.as_str(), c.role), ("brain 8/8", Role::Meta));
    }

    /// With a node's cost unknown the total is a floor, so the brain's share is only a ceiling.
    #[test]
    fn an_incomplete_cost_marks_the_share() {
        let w = BrainSelfWork {
            cost_complete: false,
            ..work(2, true, Some(1.0))
        };
        assert!(
            line(&w, 8).contains(", 25% of the known cost"),
            "{}",
            line(&w, 8)
        );
        assert_eq!(cell(&w, 8, true).text, "brain 2/8 (~25%)");
    }
}
