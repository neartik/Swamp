use crate::ids::NodeId;
use crate::journal::fold::{RunView, TreeRow};
use crate::journal::paths::RunPaths;
use crate::journal::reader::Tailer;
use crate::journal::record::JournalLine;
use crate::model::core::{NodeKind, NodeState};
use crate::model::event::WorkerEvent;
use crate::model::failure::{Detector, Failure};
use crate::model::node::NodeRecord;
use crate::ui::fmt;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use time::OffsetDateTime;

const TITLE_WIDTH: usize = 34;
const ACCOUNT_WIDTH: usize = 16;
const MODEL_WIDTH: usize = 10;

#[derive(Debug, Clone, Default)]
pub struct TraceOpts {
    pub node: Option<NodeId>,
    pub events: bool,
    pub raw: bool,
    pub stderr: bool,
    pub depth: Option<u32>,
    pub failed: bool,
    pub json: bool,
}

pub fn render(view: &RunView, o: &TraceOpts) -> String {
    if o.json {
        return render_json(view, o);
    }
    let mut out = String::new();
    out.push_str(&header(view));
    let rows = visible(view, o);
    for (i, row) in rows.iter().enumerate() {
        if row.depth > 0 {
            out.push_str(&format!("{}|\n", "  ".repeat(row.depth as usize)));
        }
        out.push_str(&block(view, row, more_siblings(&rows, i), o));
    }
    if o.node.is_none() {
        out.push_str(&footer(view));
    }
    out
}

/// Rows the flags leave standing, already filtered by depth, node and failure.
fn visible(view: &RunView, o: &TraceOpts) -> Vec<TreeRow> {
    view.tree()
        .into_iter()
        .filter(|r| o.depth.is_none_or(|d| r.depth <= d))
        .filter(|r| !o.failed || matches!(r.state, NodeState::Failed { .. }))
        .filter(|r| match o.node {
            None => true,
            Some(id) => r.logical == id || r.attempts.contains(&id),
        })
        .collect()
}

fn more_siblings(rows: &[TreeRow], i: usize) -> bool {
    let depth = rows[i].depth;
    rows[i + 1..]
        .iter()
        .find(|r| r.depth <= depth)
        .is_some_and(|r| r.depth == depth)
}

fn header(view: &RunView) -> String {
    let Some(h) = &view.header else {
        return String::new();
    };
    let where_ = h.repo.clone().unwrap_or_else(|| h.cwd.clone());
    let base = h.base.as_deref().map(fmt::short_sha).unwrap_or_default();
    let elapsed = elapsed(view)
        .map(fmt::duration)
        .unwrap_or_else(|| "-".to_owned());
    format!(
        "run {}  {where_}  base {base}  started {}  {elapsed}  {}  {} tok\n\n",
        h.run.short(),
        fmt::clock(h.started_at),
        total_cost(view),
        fmt::tokens(view.totals.billable()),
    )
}

fn footer(view: &RunView) -> String {
    let u = view.totals;
    let mut out = format!(
        "\nusage  in {}  out {}  cache-read {}  cache-write {}\n",
        fmt::tokens(u.input_tokens),
        fmt::tokens(u.output_tokens),
        fmt::tokens(u.cached_input_tokens),
        fmt::tokens(u.cache_write_tokens),
    );
    let unknown = view.nodes.values().filter(|n| n.cost.is_none()).count();
    out.push_str(&format!("cost   {}", total_cost(view)));
    if unknown > 0 {
        let plural = if unknown == 1 { "node" } else { "nodes" };
        out.push_str(&format!("   ({unknown} {plural} reported no cost data)"));
    }
    out.push('\n');
    out
}

fn total_cost(view: &RunView) -> String {
    if view.cost_usd == 0.0 && !view.cost_complete {
        return "-".to_owned();
    }
    format!("~${:.2}", view.cost_usd)
}

/// One collapsed row: the headline, its attempt chain, its failure and its branch.
fn block(view: &RunView, row: &TreeRow, siblings: bool, o: &TraceOpts) -> String {
    let Some(rec) = latest(view, row) else {
        return String::new();
    };
    let indent = "  ".repeat(row.depth as usize);
    let stem = if row.depth == 0 {
        "* ".to_owned()
    } else {
        format!("{indent}+- ")
    };
    let detail = if row.depth == 0 {
        "    ".to_owned()
    } else if siblings {
        format!("{indent}|    ")
    } else {
        format!("{indent}     ")
    };

    let tier = if rec.kind == NodeKind::Worker {
        format!("[{}] ", fmt::pad(&rec.tier.to_string(), 4))
    } else {
        String::new()
    };
    // The attempt id names the node directory and is what `diff`/`adopt` are given, so the
    // row leads with the id the user will copy.
    let mut out = format!(
        "{stem}{}  {}{}  {}  {}  {}  {}\n",
        rec.id.short(),
        tier,
        fmt::pad(&row.title, TITLE_WIDTH),
        fmt::pad(&account_cell(rec), ACCOUNT_WIDTH),
        fmt::pad(rec.model.as_deref().unwrap_or("-"), MODEL_WIDTH),
        numbers(rec),
        fmt::state_word(&row.state),
    );

    if row.attempts.len() > 1 {
        for id in &row.attempts {
            if let Some(a) = view.nodes.get(id) {
                out.push_str(&format!(
                    "{detail}attempt {}  {}  {}  {}\n",
                    a.attempt,
                    a.id.short(),
                    fmt::pad(&account_cell(a), ACCOUNT_WIDTH),
                    attempt_outcome(a),
                ));
            }
        }
    }
    if let NodeState::Failed { failure } = &row.state {
        out.push_str(&format!("{detail}{}\n", failure_detail(failure)));
        if failure.is_terminal() {
            out.push_str(&format!("{detail}no failover (task-level failure)\n"));
        }
    }
    if let Some(w) = &rec.work
        && !w.empty
    {
        // The file list comes from git; it is omitted, never reported as zero, when absent.
        let files = match rec.files.len() {
            0 => String::new(),
            1 => "   1 file".to_owned(),
            n => format!("   {n} files"),
        };
        out.push_str(&format!(
            "{detail}branch {}   +{} -{}{files}\n",
            w.branch, w.insertions, w.deletions
        ));
    }
    if o.events {
        for id in &row.attempts {
            for ev in view.events.get(id).into_iter().flatten() {
                out.push_str(&format!("{detail}  {}\n", event_text(ev)));
            }
        }
    }
    out
}

fn latest<'a>(view: &'a RunView, row: &TreeRow) -> Option<&'a NodeRecord> {
    row.attempts.iter().rev().find_map(|a| view.nodes.get(a))
}

fn account_cell(rec: &NodeRecord) -> String {
    let account = rec.account.as_ref().map_or("-", |a| a.0.as_str());
    format!("{}/{account}", rec.provider)
}

fn numbers(rec: &NodeRecord) -> String {
    format!(
        "{:>7}  {:>7}  {:>7}",
        rec.duration()
            .map(fmt::duration)
            .unwrap_or_else(|| "-".to_owned()),
        fmt::tokens(rec.usage.billable()),
        fmt::cost(rec.cost),
    )
}

fn attempt_outcome(rec: &NodeRecord) -> String {
    let tail = match &rec.state {
        NodeState::Failed { failure } => failure_summary(failure),
        s => fmt::state_word(s).to_owned(),
    };
    let model = rec.model.as_deref().unwrap_or("-");
    format!(
        "{}  {tail}  {}",
        fmt::pad(model, MODEL_WIDTH),
        rec.duration()
            .map(fmt::duration)
            .unwrap_or_else(|| "-".to_owned())
    )
}

/// The one-line reason an attempt ended, with the evidence a misclassification needs.
fn failure_summary(f: &Failure) -> String {
    match f {
        Failure::RateLimited {
            resets_at,
            scope,
            detected_by,
            ..
        } => {
            let resets = resets_at
                .map(|t| format!(" resets {}", fmt::clock_hm(t)))
                .unwrap_or_default();
            format!(
                "rate_limited ({}, {}){resets}",
                scope_word(scope),
                detector_word(detected_by)
            )
        }
        Failure::AuthExpired { detected_by, .. } => {
            format!("auth_expired ({})", detector_word(detected_by))
        }
        Failure::Overloaded { .. } => "overloaded".to_owned(),
        Failure::BudgetExceeded { .. } => "budget_exceeded".to_owned(),
        Failure::Timeout { after_s } => format!("timeout ({after_s}s)"),
        Failure::WorkerError { subtype, .. } => format!("worker_error ({subtype})"),
        Failure::PermissionDenied { denials, .. } => format!("permission_denied ({denials})"),
        Failure::Crashed { .. } => "crashed".to_owned(),
        Failure::Truncated { .. } => "truncated".to_owned(),
        Failure::NoCapacity { .. } => "no_capacity".to_owned(),
        Failure::Cancelled { .. } => "cancelled".to_owned(),
    }
}

fn failure_detail(f: &Failure) -> String {
    match f {
        Failure::RateLimited { evidence, .. } => {
            format!("{}: {}", failure_summary(f), fmt::truncate(evidence, 100))
        }
        Failure::AuthExpired { detail, .. }
        | Failure::Overloaded { detail }
        | Failure::NoCapacity { detail } => {
            format!("{}: {}", failure_summary(f), fmt::truncate(detail, 100))
        }
        Failure::WorkerError { subtype, detail } => format!(
            "WorkerError({subtype}): {}",
            fmt::truncate(detail, 100).trim()
        ),
        Failure::BudgetExceeded {
            limit_usd,
            spent_usd,
        } => format!("BudgetExceeded: spent ${spent_usd:.2} of ${limit_usd:.2}"),
        Failure::PermissionDenied { denials, tools } => {
            let which = tool_tally(tools);
            format!("PermissionDenied: {denials} tool calls were auto-denied{which}")
        }
        Failure::Crashed { signal } => match signal {
            Some(s) => format!("Crashed: killed by signal {s}"),
            None => "Crashed: the process died abnormally".to_owned(),
        },
        Failure::Truncated { offset } => {
            format!("Truncated: the stream ended at byte {offset} with no final event")
        }
        Failure::Timeout { after_s } => format!("Timeout: no terminal event after {after_s}s"),
        Failure::Cancelled { by } => format!("Cancelled by {by:?}"),
    }
}

/// `: Bash x3, Edit`, empty when the provider named no tools.
fn tool_tally(tools: &[String]) -> String {
    let mut tally: Vec<(&str, u32)> = Vec::new();
    for t in tools {
        match tally.iter_mut().find(|(name, _)| *name == t.as_str()) {
            Some((_, n)) => *n += 1,
            None => tally.push((t.as_str(), 1)),
        }
    }
    if tally.is_empty() {
        return String::new();
    }
    let listed: Vec<String> = tally
        .iter()
        .map(|(name, n)| {
            if *n > 1 {
                format!("{name} x{n}")
            } else {
                (*name).to_owned()
            }
        })
        .collect();
    format!(": {}", listed.join(", "))
}

fn scope_word(s: &crate::model::core::LimitScope) -> &'static str {
    use crate::model::core::LimitScope::*;
    match s {
        FiveHour => "five_hour",
        SevenDay => "seven_day",
        Minute => "minute",
        Unknown => "unknown",
    }
}

fn detector_word(d: &Detector) -> &'static str {
    match d {
        Detector::Telemetry => "telemetry",
        Detector::StructuredResult => "structured_result",
        Detector::Pattern => "pattern",
        Detector::ExitCode => "exit_code",
    }
}

/// One normalized worker event as a single line, shared with the live TUI.
pub fn event_text(e: &WorkerEvent) -> String {
    match e {
        WorkerEvent::SessionStarted { session, model, .. } => format!(
            "session {} {}",
            fmt::truncate(session, 12),
            model.as_deref().unwrap_or("-")
        ),
        WorkerEvent::AssistantText { text } => format!("text: {}", one_line(text, 96)),
        WorkerEvent::Thinking { text } => format!("thinking: {}", one_line(text, 96)),
        WorkerEvent::ToolCall { name, summary, .. } => {
            format!("tool {name}: {}", one_line(summary, 80))
        }
        WorkerEvent::ToolResult { ok, summary, .. } => {
            format!("tool result ok={ok} {}", one_line(summary, 60))
        }
        WorkerEvent::FileChanged { path, kind } => format!("file {kind:?} {path}"),
        WorkerEvent::Usage(u) => format!(
            "usage in {} out {}",
            fmt::tokens(u.input_tokens),
            fmt::tokens(u.output_tokens)
        ),
        WorkerEvent::RateLimit(r) => format!("rate limit {:.2}", r.worst_utilization()),
        WorkerEvent::Final(f) => format!("final {} ok={}", f.subtype, f.ok),
        WorkerEvent::Unknown { .. } => "unknown event".to_owned(),
    }
}

fn one_line(s: &str, n: usize) -> String {
    fmt::truncate(s, n)
}

/// The run's wall time, taken from the nodes so that the same journal always renders the
/// same string.
fn elapsed(view: &RunView) -> Option<std::time::Duration> {
    let started = view.header.as_ref()?.started_at;
    let last = view.nodes.values().filter_map(|n| n.ended_at).max()?;
    (last - started).try_into().ok()
}

// ---------------------------------------------------------------- json

fn render_json(view: &RunView, o: &TraceOpts) -> String {
    if let Some(id) = o.node {
        let value = view
            .nodes
            .get(&id)
            .map(|n| serde_json::to_value(n).unwrap_or(Value::Null))
            .unwrap_or(Value::Null);
        return format!("{}\n", pretty(&value));
    }
    format!("{}\n", pretty(&full_json(view)))
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// One source of truth: the text tree and `--json` are the same fold.
fn full_json(view: &RunView) -> Value {
    let mut nodes = Map::new();
    for (id, rec) in &view.nodes {
        nodes.insert(
            id.to_string(),
            serde_json::to_value(rec).unwrap_or(Value::Null),
        );
    }
    let tree: Vec<Value> = view
        .tree()
        .iter()
        .map(|r| {
            json!({
                "logical": r.logical.to_string(),
                "depth": r.depth,
                "title": r.title,
                "state": serde_json::to_value(&r.state).unwrap_or(Value::Null),
                "attempts": r.attempts.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut events = Map::new();
    for (id, evs) in &view.events {
        events.insert(
            id.to_string(),
            serde_json::to_value(evs).unwrap_or(Value::Null),
        );
    }
    let header = view.header.as_ref().map(|h| {
        json!({
            "run": h.run.to_string(),
            "swamp_version": h.swamp_version,
            "schema": h.schema,
            "argv": h.argv,
            "cwd": h.cwd,
            "repo": h.repo,
            "base": h.base,
            "config_sha256": h.config_sha256,
            "task": h.task,
            "started_at": h.started_at.to_offset(time::UtcOffset::UTC)
                .format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
        })
    });
    json!({
        "run": header,
        "finished": view.finished,
        "last_seq": view.last_seq,
        "totals": {
            "usage": view.totals,
            "cost_usd": view.cost_usd,
            "cost_complete": view.cost_complete,
        },
        "tree": tree,
        "nodes": Value::Object(nodes),
        "events": Value::Object(events),
    })
}

// ---------------------------------------------------------------- follow

/// Incremental rendering for `--follow`: a row is printed once and reprinted only when the
/// fold actually changed it.
pub struct Follower {
    pub view: RunView,
    opts: TraceOpts,
    printed: BTreeMap<NodeId, String>,
    header: bool,
}

impl Follower {
    pub fn new(opts: TraceOpts, with_events: bool) -> Self {
        let mut view = RunView::default();
        view.with_events = with_events;
        Follower {
            view,
            opts,
            printed: BTreeMap::new(),
            header: false,
        }
    }

    pub fn ingest(&mut self, lines: &[JournalLine]) -> String {
        for l in lines {
            self.view.apply(l);
        }
        let mut out = String::new();
        if !self.header && self.view.header.is_some() {
            self.header = true;
            out.push_str(&header(&self.view));
        }
        let rows = visible(&self.view, &self.opts);
        for (i, row) in rows.iter().enumerate() {
            let text = block(&self.view, row, more_siblings(&rows, i), &self.opts);
            if self.printed.get(&row.logical) != Some(&text) {
                out.push_str(&text);
                self.printed.insert(row.logical, text);
            }
        }
        out
    }

    pub fn finished(&self) -> bool {
        self.view.finished
    }
}

pub async fn follow(paths: &RunPaths, o: &TraceOpts) -> anyhow::Result<()> {
    let mut tailer = Tailer::open(&paths.journal())?;
    let mut follower = Follower::new(o.clone(), o.events);
    loop {
        let lines = tailer.poll().await?;
        let text = follower.ingest(&lines);
        if !text.is_empty() {
            let mut out = std::io::stdout().lock();
            out.write_all(text.as_bytes())?;
            out.flush()?;
        }
        if follower.finished() {
            let mut out = std::io::stdout().lock();
            out.write_all(footer(&follower.view).as_bytes())?;
            out.flush()?;
            return Ok(());
        }
    }
}

/// `--since`: drop everything older than the cutoff before rendering.
pub fn keep_since(view: &mut RunView, cutoff: OffsetDateTime) {
    let mut keep: Vec<NodeId> = view
        .nodes
        .iter()
        .filter(|(_, n)| n.created_at >= cutoff)
        .map(|(id, _)| *id)
        .collect();
    // `tree()` walks down from the roots, so dropping an ancestor would hide every recent
    // node under it. The brain node is older than everything it dispatched.
    let mut at = 0;
    while at < keep.len() {
        let parent = view.nodes.get(&keep[at]).and_then(|n| n.parent);
        at += 1;
        if let Some(p) = parent
            && view.nodes.contains_key(&p)
            && !keep.contains(&p)
        {
            keep.push(p);
        }
    }
    view.nodes.retain(|id, _| keep.contains(id));
    view.roots.retain(|id| keep.contains(id));
    view.children.retain(|p, _| keep.contains(p));
    for kids in view.children.values_mut() {
        kids.retain(|k| keep.contains(k));
    }
    view.by_logical.retain(|_, chain| {
        chain.retain(|a| keep.contains(a));
        !chain.is_empty()
    });
    view.events.retain(|id, _| keep.contains(id));
    // Totals describe what is rendered; without this the footer reports the whole run.
    view.recompute();
}
