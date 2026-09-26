//! `swamp dispatches` and `swamp dispatch <ID>`: the dispatch surface of a run, as text or as
//! the JSON shape `journal::inspect` defines.

use crate::ids::DispatchId;
use crate::journal::fold::RunView;
use crate::journal::inspect::{self, DispatchSummary, TaskDetail};
use crate::journal::paths::RunPaths;
use crate::journal::reader::Tailer;
use crate::ui::fmt;
use crate::ui::trace::failure_detail;
use std::collections::BTreeMap;
use std::io::Write;
use std::time::Duration;
use time::OffsetDateTime;

const TITLE_WIDTH: usize = 34;
const ACCOUNT_WIDTH: usize = 16;
const MODEL_WIDTH: usize = 10;
/// A dispatch tree deeper than this is a cycle in a damaged journal, not real nesting.
const MAX_NESTING: usize = 8;

#[derive(Debug, Clone, Copy, Default)]
pub struct ListOpts {
    /// Only dispatches with a failed or rejected task.
    pub failed: bool,
    pub json: bool,
}

pub fn render_list(view: &RunView, o: ListOpts, now: OffsetDateTime) -> String {
    let mut list = inspect::list(view, now);
    list.dispatches.retain(|d| keep(d, o));
    if o.json {
        return format!("{}\n", pretty(&list));
    }
    let run = view
        .header
        .as_ref()
        .map_or_else(|| "?".to_owned(), |h| h.run.short());
    let mut out = format!("run {run}  {} dispatches\n\n", list.dispatches.len());
    if list.dispatches.is_empty() {
        out.push_str("no dispatches recorded\n");
        return out;
    }
    out.push_str(&heading());
    for d in &list.dispatches {
        out.push_str(&row(d));
    }
    out
}

fn keep(d: &DispatchSummary, o: ListOpts) -> bool {
    !o.failed || d.counts.failed + d.counts.rejected > 0
}

fn heading() -> String {
    format!(
        "{:<9} {:>4}  {:>7}  {:<7}  {:>5}  {:>4}  {:>3}  {:>3}  {:>4}  {:>4}  {:>3}  {:>8}  CALLER\n",
        "DISPATCH",
        "SEQ",
        "AGE",
        "STATE",
        "TASKS",
        "WAIT",
        "RUN",
        "OK",
        "FAIL",
        "CANC",
        "REJ",
        "COST"
    )
}

/// One dispatch: seq, age, tasks, the per-state counts, cost and who asked.
pub fn row(d: &DispatchSummary) -> String {
    let c = d.counts;
    format!(
        "{:<9} {:>4}  {:>7}  {:<7}  {:>5}  {:>4}  {:>3}  {:>3}  {:>4}  {:>4}  {:>3}  {:>8}  {}\n",
        d.short,
        d.call_seq.map_or_else(|| "-".to_owned(), |s| s.to_string()),
        d.age_s
            .map_or_else(|| "-".to_owned(), |s| fmt::duration(Duration::from_secs(s))),
        state_word(d),
        d.tasks,
        c.waiting(),
        c.running,
        c.succeeded,
        c.failed,
        c.cancelled,
        c.rejected,
        cost(&d.cost),
        caller_word(d),
    )
}

pub fn state_word(d: &DispatchSummary) -> &'static str {
    match d.state {
        crate::model::dispatch::DispatchState::Open => "open",
        crate::model::dispatch::DispatchState::Settled => "settled",
    }
}

/// "1 task", "3 tasks".
pub fn tasks_word(n: u32) -> String {
    if n == 1 {
        "1 task".to_owned()
    } else {
        format!("{n} tasks")
    }
}

pub fn caller_word(d: &DispatchSummary) -> String {
    match &d.caller {
        None => "-".to_owned(),
        Some(c) if c.kind == "brain" => "brain".to_owned(),
        Some(c) => format!("task {}", c.task.unwrap_or(c.node).short()),
    }
}

/// "-" when nothing ran or nothing reported a cost; a trailing "+" when some attempt did not.
pub fn cost(r: &inspect::Rollup) -> String {
    if r.usd == 0.0 && (!r.complete || r.usage.billable() == 0) {
        return "-".to_owned();
    }
    let plus = if r.complete { "" } else { "+" };
    format!("~${:.2}{plus}", r.usd)
}

pub fn render_detail(view: &RunView, id: DispatchId, json: bool, now: OffsetDateTime) -> String {
    let Some(detail) = inspect::detail(view, id, now) else {
        return format!("no dispatch {} in this run\n", inspect::label(id));
    };
    if json {
        return format!("{}\n", pretty(&detail));
    }
    let d = &detail.dispatch;
    let run = view
        .header
        .as_ref()
        .map_or_else(|| "?".to_owned(), |h| h.run.short());
    let mut out = format!(
        "dispatch {}  run {run}  seq {}  {}  caller {}",
        d.short,
        d.call_seq.map_or_else(|| "-".to_owned(), |s| s.to_string()),
        state_word(d),
        caller_word(d),
    );
    if let Some(age) = d.age_s {
        out.push_str(&format!(
            "  {} ago",
            fmt::duration(Duration::from_secs(age))
        ));
    }
    out.push('\n');
    let c = d.counts;
    out.push_str(&format!(
        "{}  waiting {}  running {}  ok {}  failed {}  cancelled {}  rejected {}  cost {}\n\n",
        tasks_word(d.tasks),
        c.waiting(),
        c.running,
        c.succeeded,
        c.failed,
        c.cancelled,
        c.rejected,
        cost(&d.cost),
    ));
    for t in &detail.tasks {
        task_block(view, t, 0, now, &mut out);
    }
    out
}

/// A task, its attempts, why it is waiting or was refused, and whatever it dispatched itself.
fn task_block(view: &RunView, t: &TaskDetail, level: usize, now: OffsetDateTime, out: &mut String) {
    let indent = "    ".repeat(level);
    let detail = format!("{indent}    ");
    let attempts = match t.attempts.len() {
        0 => String::new(),
        1 => "  1 attempt".to_owned(),
        n => format!("  {n} attempts"),
    };
    out.push_str(&format!(
        "{indent}* {}  [{}] {}  {}  {}{attempts}\n",
        t.node.short(),
        fmt::pad(&t.tier.to_string(), 4),
        fmt::pad(&t.title, TITLE_WIDTH),
        fmt::pad(fmt::state_word(&t.detail), 9),
        cost(&t.cost),
    ));
    for a in &t.attempts {
        let account = a.account.as_ref().map_or("-", |a| a.0.as_str());
        let outcome = match &a.failure {
            Some(f) => crate::ui::trace::failure_summary(f),
            None => fmt::phase_word(a.state).to_owned(),
        };
        let pid = a.pid.map(|p| format!("  pid {p}")).unwrap_or_default();
        out.push_str(&format!(
            "{detail}attempt {}  {}  {}  {}  {:>7}  {outcome}{pid}\n",
            a.attempt,
            a.node.short(),
            fmt::pad(&format!("{}/{account}", a.provider), ACCOUNT_WIDTH),
            fmt::pad(a.model.as_deref().unwrap_or("-"), MODEL_WIDTH),
            a.elapsed_ms.map_or_else(
                || "-".to_owned(),
                |ms| fmt::duration(Duration::from_millis(ms))
            ),
        ));
    }
    if let Some(reason) = &t.rejected {
        out.push_str(&format!("{detail}rejected: {}\n", failure_detail(reason)));
    }
    if let Some(f) = &t.failure {
        out.push_str(&format!("{detail}{}\n", failure_detail(f)));
    }
    if let Some(b) = &t.blocked {
        out.push_str(&format!(
            "{detail}blocked until {}: {}\n",
            fmt::clock_day(b.until, now),
            b.why
        ));
        for r in &b.ineligible {
            out.push_str(&format!("{detail}  {}: {}\n", r.account.0, r.reason.word()));
        }
    }
    if level >= MAX_NESTING {
        return;
    }
    for nested in inspect::issued_by(view, t.node) {
        let Some(sub) = inspect::detail(view, nested, now) else {
            continue;
        };
        out.push_str(&format!(
            "{detail}dispatch {}  {}  {}\n",
            sub.dispatch.short,
            tasks_word(sub.dispatch.tasks),
            state_word(&sub.dispatch)
        ));
        for st in &sub.tasks {
            task_block(view, st, level + 1, now, out);
        }
    }
}

fn pretty<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// `--follow`: a row is printed when it first appears and again whenever it changes, until
/// the run finishes.
pub struct Follower {
    pub view: RunView,
    opts: ListOpts,
    printed: BTreeMap<DispatchId, String>,
    heading: bool,
}

impl Follower {
    pub fn new(opts: ListOpts) -> Self {
        Follower {
            view: RunView::default(),
            opts,
            printed: BTreeMap::new(),
            heading: false,
        }
    }

    pub fn ingest(&mut self, lines: &[crate::journal::JournalLine], now: OffsetDateTime) -> String {
        for l in lines {
            self.view.apply(l);
        }
        let mut out = String::new();
        for d in self.view.dispatches.values() {
            let s = inspect::summary(&self.view, d, now);
            if !keep(&s, self.opts) {
                continue;
            }
            // Age alone changes every tick; it is not a reason to reprint.
            let key = format!("{:?}{:?}{}", s.state, s.counts, cost(&s.cost));
            if self.printed.get(&d.id) == Some(&key) {
                continue;
            }
            self.printed.insert(d.id, key);
            if self.opts.json {
                out.push_str(&serde_json::to_string(&s).unwrap_or_default());
                out.push('\n');
            } else {
                if !self.heading {
                    self.heading = true;
                    out.push_str(&heading());
                }
                out.push_str(&row(&s));
            }
        }
        out
    }

    pub fn finished(&self) -> bool {
        self.view.finished
    }
}

pub async fn follow(paths: &RunPaths, opts: ListOpts) -> anyhow::Result<()> {
    let mut tailer = Tailer::open(&paths.journal())?;
    let mut follower = Follower::new(opts);
    loop {
        let lines = tailer.poll().await?;
        let text = follower.ingest(&lines, OffsetDateTime::now_utc());
        if !text.is_empty() {
            let mut out = std::io::stdout().lock();
            out.write_all(text.as_bytes())?;
            out.flush()?;
        }
        if follower.finished() {
            return Ok(());
        }
    }
}
