use crate::cli::ResumeArgs;
use crate::cmd::{Ctx, RunSession, write_result};
use crate::ids::{DispatchId, NodeId, NodeIds};
use crate::journal::fold::RunView;
use crate::journal::paths::RunPaths;
use crate::journal::record::JournalEvent;
use crate::model::core::{
    CancelSource, Cost, NodeKind, NodeState, SessionHandle, Usage, WorkspaceRef,
};
use crate::model::dispatch::{DispatchCounts, DispatchState, Phase};
use crate::model::failure::Failure;
use crate::model::node::NodeRecord;
use crate::model::result::{IsolationMode, NodeResult};
use crate::worker::adapter::{LaunchSpec, SessionPlan};
use crate::workspace::NodeWorktree;
use camino::Utf8PathBuf;

/// What a node needs before the run can continue. Nothing here auto-runs: relaunching a
/// worker spends quota, so the plan is printed and the user decides.
#[derive(Debug, Clone)]
pub enum Recovery {
    /// pid and start time still match: re-attach the tailer at `offset`.
    Adopt { node: NodeId, pid: i32, offset: u64 },
    /// Process gone, stream on disk: finalize from what was recorded.
    Finalize { node: NodeId, offset: u64 },
    /// Process gone, stream truncated, session handle known.
    ResumeSession {
        node: NodeId,
        session: SessionHandle,
        continuation: String,
    },
    /// Process gone, no session handle: rerun from the original prompt.
    Rerun { node: NodeId },
    /// Uncommitted work the node cannot continue: hand it to the user.
    Salvage { node: NodeId, path: Utf8PathBuf },
}

impl Recovery {
    pub fn node(&self) -> NodeId {
        match self {
            Recovery::Adopt { node, .. }
            | Recovery::Finalize { node, .. }
            | Recovery::ResumeSession { node, .. }
            | Recovery::Rerun { node }
            | Recovery::Salvage { node, .. } => *node,
        }
    }

    fn describe(&self, view: &RunView) -> String {
        let title = view
            .nodes
            .get(&self.node())
            .map(|n| n.title.clone())
            .unwrap_or_default();
        match self {
            Recovery::Adopt { node, pid, offset } => format!(
                "adopt      {} {title}: pid {pid} is alive, resume the tailer at stream_offset {offset}",
                node.short()
            ),
            Recovery::Finalize { node, offset } => format!(
                "finalize   {} {title}: the process is gone, finalize from stream_offset {offset}",
                node.short()
            ),
            Recovery::ResumeSession { node, session, .. } => format!(
                "resume     {} {title}: relaunch with session {} on account {}",
                node.short(),
                session.id,
                session.account.0
            ),
            Recovery::Rerun { node } => {
                format!(
                    "rerun      {} {title}: no session handle, rerun the prompt",
                    node.short()
                )
            }
            Recovery::Salvage { node, path } => {
                format!(
                    "salvage    {} {title}: uncommitted work in {path}",
                    node.short()
                )
            }
        }
    }
}

/// Recover an interrupted run.
pub async fn run(ctx: &Ctx, args: &ResumeArgs) -> anyhow::Result<i32> {
    crate::cmd::guard_depth(&ctx.cfg)?;
    let paths = ctx.run_paths(args.run.as_deref())?;
    let view = ctx.view(&paths, false)?;
    let wanted = |id: NodeId| args.only.is_empty() || args.only.iter().any(|spec| id.matches(spec));
    let mut steps = plan(&view, &paths);
    steps.retain(|s| wanted(s.node()));
    let ends: Vec<TaskEnd> = open_tasks(&view)
        .into_iter()
        .filter(|e| wanted(e.logical))
        .collect();

    if steps.is_empty() && ends.is_empty() && open_dispatches(&view).is_empty() {
        println!("run {} has nothing to recover", paths.run);
        return Ok(0);
    }
    if args.plan {
        let mut text = format!("recovery plan for run {}\n", paths.run);
        for s in &steps {
            text.push_str(&format!("  {}\n", s.describe(&view)));
        }
        for end in &ends {
            text.push_str(&format!("  {}\n", end.describe(&view)));
        }
        text.push_str("nothing was started: rerun without --plan to act on this plan\n");
        ctx.out(&text);
        return Ok(0);
    }

    let session = RunSession::start(ctx, ctx.cfg.clone(), paths.run, None).await?;
    let mut failed = 0;
    let mut usage = Usage::default();
    for step in &steps {
        match step {
            Recovery::Adopt { node, pid, offset } => {
                match attach(&session, &view, *node, Some(*pid), *offset).await {
                    Ok(u) => usage.absorb(&u),
                    Err(e) => {
                        eprintln!("could not recover node {}: {e:#}", node.short());
                        failed += 1;
                    }
                }
            }
            Recovery::Finalize { node, offset } => {
                match attach(&session, &view, *node, None, *offset).await {
                    Ok(u) => usage.absorb(&u),
                    Err(e) => {
                        eprintln!("could not finalize node {}: {e:#}", node.short());
                        failed += 1;
                    }
                }
            }
            other if args.rerun_failed => {
                println!(
                    "{}: rerun is not wired to a brainless run yet",
                    other.describe(&view)
                );
                failed += 1;
            }
            other => println!("skipped: {}", other.describe(&view)),
        }
    }

    close_open(ctx, &session, &paths, &wanted).await?;
    let totals = ctx.view(&paths, false)?.totals();
    session
        .finish(
            if failed > 0 {
                NodeState::Failed {
                    failure: Failure::WorkerError {
                        subtype: "resume_incomplete".into(),
                        detail: format!("{failed} nodes could not be recovered"),
                    },
                }
            } else {
                NodeState::Succeeded
            },
            totals.nodes,
            totals.usage,
            Some(totals.cost_usd),
        )
        .await?;
    Ok(if failed > 0 { 4 } else { 0 })
}

/// Workers are detached, so the common case after a supervisor crash is that the worker
/// never noticed and is still running.
pub fn plan(view: &RunView, paths: &RunPaths) -> Vec<Recovery> {
    let mut out = Vec::new();
    for (id, node) in &view.nodes {
        if node.state.is_terminal() || node.kind == NodeKind::Brain {
            continue;
        }
        let alive = paths.is_live(*id);
        let pid = match &node.state {
            NodeState::Running { pid, .. } | NodeState::Orphaned { pid, .. } => *pid,
            _ => 0,
        };
        let stream = paths.stream(*id);
        let bytes = std::fs::metadata(&stream).map(|m| m.len()).unwrap_or(0);

        if alive && pid > 0 {
            out.push(Recovery::Adopt {
                node: *id,
                pid,
                offset: node.stream_offset,
            });
        } else if bytes > 0 {
            out.push(Recovery::Finalize {
                node: *id,
                offset: node.stream_offset,
            });
        } else if let Some(session) = node.session.clone() {
            out.push(Recovery::ResumeSession {
                node: *id,
                session,
                continuation: format!(
                    "resuming `{}` after a supervisor restart; continue where you left off",
                    node.title
                ),
            });
        } else if dirty(node) {
            out.push(Recovery::Salvage {
                node: *id,
                path: node.workspace.path().to_path_buf(),
            });
        } else {
            out.push(Recovery::Rerun { node: *id });
        }
    }
    out
}

/// A task that nothing will ever move again: no attempt of it is left to recover.
#[derive(Debug, Clone)]
pub struct TaskEnd {
    pub logical: NodeId,
    pub from: Phase,
    pub to: NodeState,
}

impl TaskEnd {
    fn describe(&self, view: &RunView) -> String {
        let title = view
            .tasks
            .get(&self.logical)
            .map(|t| t.title.clone())
            .unwrap_or_default();
        format!(
            "end        {} {title}: the supervisor is gone, the task ends {}",
            self.logical.short(),
            crate::ui::fmt::state_word(&self.to)
        )
    }
}

/// Open tasks with no live attempt: they end as their last attempt did, or cancelled.
pub fn open_tasks(view: &RunView) -> Vec<TaskEnd> {
    view.tasks
        .keys()
        .filter_map(|logical| {
            let state = view.state_of(*logical)?;
            let attempts = view.attempts(*logical);
            if state.is_terminal() || attempts.iter().any(|a| !a.state.is_terminal()) {
                return None;
            }
            let to = attempts.last().map_or(
                NodeState::Cancelled {
                    by: CancelSource::Shutdown,
                },
                |a| a.state.clone(),
            );
            Some(TaskEnd {
                logical: *logical,
                from: Phase::from(&state),
                to,
            })
        })
        .collect()
}

/// Open dispatches whose every task has ended, with what their `DispatchSettled` carries.
pub fn open_dispatches(view: &RunView) -> Vec<(NodeId, DispatchId, DispatchCounts, Option<Cost>)> {
    view.dispatches
        .values()
        .filter(|d| d.id != DispatchId::LEGACY && d.state == DispatchState::Open)
        .filter_map(|d| {
            let caller = d.record.as_ref()?.caller;
            let mut counts = DispatchCounts::default();
            for t in &d.tasks {
                let state = view.state_of(*t).filter(NodeState::is_terminal)?;
                counts.count(&state);
            }
            let cost = crate::dispatch::total_cost(
                d.tasks
                    .iter()
                    .flat_map(|t| view.attempts(*t))
                    .map(|a| a.cost),
            );
            Some((caller, d.id, counts, cost))
        })
        .collect()
}

/// Ends the tasks the crash stranded, then settles the dispatches they close.
async fn close_open(
    ctx: &Ctx,
    session: &RunSession,
    paths: &RunPaths,
    wanted: &dyn Fn(NodeId) -> bool,
) -> anyhow::Result<()> {
    let view = ctx.view(paths, false)?;
    for end in open_tasks(&view).into_iter().filter(|e| wanted(e.logical)) {
        session
            .journal
            .emit_durable(
                Some(end.logical),
                JournalEvent::NodeStateChanged {
                    from: end.from,
                    to: end.to.clone(),
                    why: "interrupted".into(),
                },
            )
            .await?;
        println!(
            "ended task {} as {}",
            end.logical.short(),
            crate::ui::fmt::state_word(&end.to)
        );
    }
    let view = ctx.view(paths, false)?;
    for (caller, dispatch, counts, cost) in open_dispatches(&view) {
        session
            .journal
            .emit_durable(
                Some(caller),
                JournalEvent::DispatchSettled {
                    dispatch,
                    counts,
                    cost,
                },
            )
            .await?;
    }
    Ok(())
}

fn dirty(node: &NodeRecord) -> bool {
    let path = node.workspace.path();
    path.is_dir()
        && std::fs::read_dir(path)
            .map(|d| d.count() > 1)
            .unwrap_or(false)
}

/// Re-attach to a worker: follow its raw stream from the journaled offset, so nothing is
/// parsed twice and the fold sees no duplicated events.
async fn attach(
    session: &RunSession,
    view: &RunView,
    node: NodeId,
    pid: Option<i32>,
    offset: u64,
) -> anyhow::Result<Usage> {
    let record = view
        .nodes
        .get(&node)
        .ok_or_else(|| anyhow::anyhow!("node {node} is not in the journal"))?;
    let out = match pid {
        Some(pid) if pid > 0 => {
            let spec = spec_for(session, record);
            session.exec.resume_from(&spec, pid, offset).await?
        }
        // No live process to wait on: the stream on disk is the whole story.
        _ => finalize_offline(session, record)?,
    };

    // A limit learned while recovering is still a limit: without this the next run leases the
    // same account and burns a node against a live 429.
    if let Some(account) = &record.account {
        session.pool.report(account, out.failure.as_ref(), out.cost);
        // A recovered node spent real tokens: without this the account's window and lifetime
        // counters under-report it and every share-based score is skewed in its favour.
        session.pool.commit_usage(account, node, out.account_usage);
        if let Some(snap) = out.rate_limit.clone() {
            session.pool.observe_quota(account, snap);
        }
    }

    let state = match &out.failure {
        None => NodeState::Succeeded,
        Some(f) => NodeState::Failed { failure: f.clone() },
    };
    let work = match (&out.failure, &record.workspace) {
        (
            None,
            WorkspaceRef::Worktree {
                path, branch, base, ..
            },
        ) => {
            let wt = NodeWorktree {
                node: record.logical,
                path: path.clone(),
                branch: branch.clone(),
                base: base.clone(),
            };
            session
                .workspace
                .finalize(&wt, &record.title, record.tier)
                .await
                .unwrap_or_default()
        }
        _ => record.work.clone(),
    };

    session
        .journal
        .emit_durable(
            Some(node),
            JournalEvent::NodeFinished {
                state: state.clone(),
                exit: out.exit,
                usage: out.usage,
                cost: out.cost,
                work: work.clone(),
                summary: out.summary.clone(),
                files: out.files.clone(),
                unparsed_lines: out.unparsed_lines,
            },
        )
        .await?;
    let why = format!(
        "recovered: {}",
        out.failure.as_ref().map_or("succeeded", Failure::kind)
    );
    session
        .journal
        .emit_durable(
            Some(node),
            JournalEvent::NodeStateChanged {
                from: Phase::from(&record.state),
                to: state.clone(),
                why: why.clone(),
            },
        )
        .await?;
    // Recovery never retries, so the recovered attempt ends its task too.
    if let Some(task) = view
        .tasks
        .get(&record.logical)
        .and_then(|t| t.state.as_ref())
        .filter(|s| !s.is_terminal())
    {
        session
            .journal
            .emit_durable(
                Some(record.logical),
                JournalEvent::NodeStateChanged {
                    from: Phase::from(task),
                    to: state.clone(),
                    why,
                },
            )
            .await?;
    }

    write_result(
        &session.paths,
        &NodeResult {
            node,
            title: record.title.clone(),
            ok: out.failure.is_none(),
            state: if out.failure.is_none() {
                "succeeded"
            } else {
                "failed"
            },
            tier: record.tier,
            provider: record.provider,
            account: record.account.clone(),
            model: record.model.clone(),
            attempts: record.attempt,
            summary: out.summary.clone(),
            files: out.files.clone(),
            branch: work.as_ref().map(|w| w.branch.clone()),
            patch: work.as_ref().map(|w| w.patch.clone()),
            insertions: work.as_ref().map_or(0, |w| w.insertions),
            deletions: work.as_ref().map_or(0, |w| w.deletions),
            usage: out.usage,
            cost: out.cost,
            duration_ms: out.exit.map_or(0, |e| e.duration_ms),
            failure: out.failure.clone(),
            permission_denials: out.permission_denials,
        },
    );
    println!(
        "recovered node {} as {}",
        node.short(),
        crate::ui::fmt::state_word(&state)
    );
    Ok(out.usage)
}

/// Classify a dead node from its retained raw stream, with no process to supervise.
fn finalize_offline(
    session: &RunSession,
    record: &NodeRecord,
) -> anyhow::Result<crate::worker::RunOutcome> {
    use crate::worker::adapter::{ExitContext, ParseState, adapter_for};
    let adapter = adapter_for(record.provider);
    let patterns = session.cfg.failure_patterns(record.provider)?;
    let raw = std::fs::read_to_string(session.paths.stream(record.id)).unwrap_or_default();
    let mut st = ParseState::default();
    let mut offset = 0u64;
    for line in raw.split_inclusive('\n') {
        offset += line.len() as u64;
        let out = adapter.parse_line(line.trim_end_matches(['\n', '\r']), &mut st);
        if out.noise {
            st.unparsed += 1;
        }
    }
    let failure = adapter.classify(&ExitContext {
        exit: record.exit,
        state: &st,
        patterns: &patterns,
        deadline_hit: false,
    });
    let cost = st.last_final.as_ref().and_then(|f| f.cost).or_else(|| {
        session
            .cfg
            .estimate_cost(record.model.as_deref().unwrap_or(""), &st.usage)
    });
    Ok(crate::worker::RunOutcome {
        failure,
        exit: record.exit,
        session: record.session.clone(),
        usage: st.usage,
        account_usage: crate::worker::account_total(&st),
        cost,
        summary: st.last_final.as_ref().and_then(|f| f.text.clone()),
        files: st.files.clone(),
        rate_limit: st.last_rate_limit.clone(),
        stream_offset: offset,
        unparsed_lines: st.unparsed,
        permission_denials: st.last_final.map_or(0, |f| f.permission_denials),
    })
}

fn spec_for(session: &RunSession, record: &NodeRecord) -> LaunchSpec {
    let worker = session
        .cfg
        .providers
        .get(&record.provider)
        .map(|p| p.worker.clone())
        .unwrap_or_default();
    let env = record
        .account
        .as_ref()
        .and_then(|a| session.cfg.account(a))
        .map(|a| crate::config::resolve::expand_env(&a.env))
        .unwrap_or_default();
    let isolation = session
        .cfg
        .workspace
        .isolation
        .unwrap_or(IsolationMode::Worktree);
    LaunchSpec {
        node: NodeIds {
            id: record.id,
            session_uuid: uuid::Uuid::new_v4(),
        },
        provider: record.provider,
        exec: record.exec.clone().unwrap_or_default(),
        env,
        model: record.model.clone().unwrap_or_default(),
        tier: record.tier,
        cwd: record.workspace.path().to_path_buf(),
        isolation,
        session: match record.session.clone() {
            Some(h) => SessionPlan::Resume(h),
            None => SessionPlan::New { preassigned: None },
        },
        kind: record.kind,
        permission_mode: worker.permission_mode.clone().unwrap_or_default(),
        sandbox: worker.sandbox.clone().unwrap_or_default(),
        append_system_prompt: None,
        allow_tools: worker.allow_tools.clone(),
        deny_tools: worker.deny_tools.clone(),
        mcp: None,
        last_message_path: session.paths.last_message(record.id),
        extra_args: worker.args_for(isolation),
        extra: session.cfg.tier_extra(record.provider, record.tier),
        partial_messages: false,
        attempt: record.attempt,
    }
}
