use crate::config::Config;
use crate::dispatch::pool::{AccountPool, Lease, NoCapacity, instant_of};
use crate::ids::{NodeId, NodeIds};
use crate::journal::{JournalEvent, JournalHandle};
use crate::model::core::{AccountId, NodeState, Provider, SessionHandle, Tier, WorkspaceRef};
use crate::model::failure::Failure;
use crate::model::node::NodeRecord;
use crate::model::result::TaskRequest;
use crate::worker::RunOutcome;
use crate::worker::adapter::{LaunchSpec, SessionPlan};
use crate::workspace::NodeWorktree;
use async_trait::async_trait;
use camino::Utf8PathBuf;
use rand::Rng;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(120);

/// The seam between policy and the two things the attempt loop cannot fake: a worktree and a
/// process. WP3 and WP5 supply the real one, tests supply a scripted one.
#[async_trait]
pub trait NodeRunner: Send + Sync + 'static {
    async fn workspace(&self, logical: NodeId, attempt: u32) -> anyhow::Result<NodeWorktree>;
    async fn run(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> anyhow::Result<RunOutcome>;
    async fn finalize(
        &self,
        wt: &NodeWorktree,
        title: &str,
        tier: Tier,
    ) -> anyhow::Result<Option<crate::model::node::WorkResultRef>>;
}

/// Everything the attempt loop needs, so the loop itself stays a pure policy statement.
pub struct NodeCtx {
    pub cfg: Arc<Config>,
    pub pool: Arc<AccountPool>,
    pub runner: Arc<dyn NodeRunner>,
    pub journal: JournalHandle,
    pub provider_order: Vec<Provider>,
    pub cross_provider: bool,
    pub max_attempts: u32,
    pub deadline: Instant,
    pub parent: Option<NodeId>,
    /// Stable across attempts: every attempt is its own node grouped under this id.
    pub logical: NodeId,
    pub cancel: CancellationToken,
}

impl NodeCtx {
    pub fn new_node_ids(&self) -> NodeIds {
        NodeIds {
            id: NodeId::new(),
            session_uuid: uuid::Uuid::new_v4(),
        }
    }

    /// Only providers that accept a caller-chosen session id get one preassigned.
    pub fn new_session_id(&self, p: Provider) -> Option<String> {
        match p {
            Provider::Anthropic => Some(uuid::Uuid::new_v4().to_string()),
            Provider::Openai => None,
        }
    }
}

/// The record of every attempt made for one logical node, plus the outcome that stands.
pub struct NodeOutcome {
    pub logical: NodeId,
    pub attempts: Vec<NodeRecord>,
    pub outcome: Option<RunOutcome>,
    pub failure: Option<Failure>,
}

impl NodeOutcome {
    pub fn ok(out: RunOutcome) -> Self {
        Self {
            logical: NodeId::new(),
            attempts: Vec::new(),
            failure: out.failure.clone(),
            outcome: Some(out),
        }
    }
    pub fn failed(f: Failure) -> Self {
        Self {
            logical: NodeId::new(),
            attempts: Vec::new(),
            outcome: None,
            failure: Some(f),
        }
    }
}

impl From<RunOutcome> for NodeOutcome {
    fn from(out: RunOutcome) -> Self {
        Self::ok(out)
    }
}

pub async fn run_node(cx: &NodeCtx, mut spec: LaunchSpec, task: &TaskRequest) -> NodeOutcome {
    let logical = cx.logical;
    let mut excluded: HashSet<AccountId> = HashSet::new();
    let mut providers = cx.provider_order.iter().copied();
    let mut provider = providers.next().unwrap_or(spec.provider);
    let mut backoff = INITIAL_BACKOFF;
    let mut prev: Option<NodeId> = None;
    let mut attempts: Vec<NodeRecord> = Vec::new();
    // Carried across a same-account retry so the backoff cannot lose the slot to a sibling.
    let mut held: Option<Lease> = None;

    for attempt in 1..=cx.max_attempts {
        if cx.cancel.is_cancelled() {
            return cancelled(logical, attempts);
        }
        let lease = match held.take() {
            Some(lease) => lease,
            None => match cx.pool.acquire(provider, &excluded, cx.deadline).await {
                Ok(l) => l,
                Err(NoCapacity::AllCooling { retry_at }) => {
                    // Cross-provider failover happens only here, and only if opted in.
                    if cx.cross_provider
                        && let Some(next) = providers.next()
                    {
                        emit(
                            cx,
                            JournalEvent::ProviderSwitch {
                                from: provider,
                                to: next,
                            },
                        );
                        provider = next;
                        excluded.clear();
                        continue;
                    }
                    let at = instant_of(retry_at);
                    if at >= cx.deadline {
                        return give_up(cx, logical, attempts, provider, &excluded);
                    }
                    emit(
                        cx,
                        JournalEvent::NodeBlocked {
                            until: retry_at,
                            why: "all accounts cooling".into(),
                        },
                    );
                    tokio::time::sleep_until(at).await;
                    continue;
                }
                Err(_) => return give_up(cx, logical, attempts, provider, &excluded),
            },
        };

        // A session handle is only valid for the account that minted it.
        spec.session = match &spec.session {
            SessionPlan::Resume(h) if h.account == lease.account => spec.session.clone(),
            SessionPlan::Resume(_) => SessionPlan::New {
                preassigned: cx.new_session_id(provider),
            },
            s => s.clone(),
        };
        spec.provider = provider;
        spec.exec = lease.exec.clone();
        spec.env = lease.env.clone();
        spec.attempt = attempt;
        spec.model = match cx.cfg.model_for(provider, spec.tier, Some(&lease.account)) {
            Ok(m) => m,
            Err(e) => {
                return fail(logical, attempts, setup_failed(&e));
            }
        };

        // A FRESH worktree per attempt: retrying on top of a half-edited tree is how you get
        // plausible-looking corruption that no test catches.
        let wt = match cx.runner.workspace(logical, attempt).await {
            Ok(w) => w,
            Err(e) => {
                return fail(logical, attempts, setup_failed(&e));
            }
        };
        emit(
            cx,
            JournalEvent::WorktreeCreated {
                path: wt.path.clone(),
                branch: wt.branch.clone(),
                base: wt.base.clone(),
            },
        );
        spec.cwd = wt.path.clone();
        spec.node = cx.new_node_ids();
        let dir = node_dir(cx, spec.node.id);
        spec.last_message_path = dir.join("last-message.txt");

        let max_prompt = cx.cfg.limits.max_prompt_bytes.unwrap_or(usize::MAX);
        let (prompt_path, prompt_sha256) = match write_prompt(&dir, &task.prompt, max_prompt) {
            Ok(v) => v,
            Err(e) => {
                return fail(logical, attempts, setup_failed(&e));
            }
        };

        emit(
            cx,
            JournalEvent::ModelResolved {
                tier: spec.tier,
                model: spec.model.clone(),
                extra: cx.cfg.tier_extra(provider, spec.tier),
            },
        );

        let mut record = NodeRecord {
            id: spec.node.id,
            run_id: cx.journal.run,
            parent: cx.parent,
            logical,
            attempt,
            retry_of: prev,
            kind: spec.kind,
            title: task.title.clone(),
            prompt_path,
            prompt_sha256,
            provider,
            account: Some(lease.account.clone()),
            exec: Some(lease.exec.clone()),
            argv: Vec::new(),
            model: Some(spec.model.clone()),
            tier: spec.tier,
            workspace: WorkspaceRef::Worktree {
                path: wt.path.clone(),
                branch: wt.branch.clone(),
                base: wt.base.clone(),
            },
            session: session_handle(&spec.session, &lease.account),
            state: NodeState::Leased {
                account: lease.account.clone(),
            },
            created_at: time::OffsetDateTime::now_utc(),
            started_at: Some(time::OffsetDateTime::now_utc()),
            ended_at: None,
            usage: Default::default(),
            cost: None,
            exit: None,
            files: Vec::new(),
            work: None,
            summary: None,
            stream_offset: 0,
            unparsed_lines: 0,
        };
        // Journaled durably BEFORE spawning, so a crash still leaves a node with full provenance.
        if let Err(e) = cx
            .journal
            .emit_durable(
                Some(spec.node.id),
                JournalEvent::NodeSpawned {
                    node: Box::new(record.clone()),
                },
            )
            .await
        {
            tracing::warn!(node = %spec.node.id.short(), "cannot journal NodeSpawned: {e}");
        }

        let timeout = cx.cfg.node_timeout(spec.tier);
        let mut out = match cx.runner.run(&spec, timeout, cx.cancel.clone()).await {
            Ok(out) => out,
            Err(e) => crashed_outcome(e),
        };
        // A session handle is only usable with the account that minted it, and the worker does
        // not know which one that was.
        if let Some(s) = out.session.as_mut() {
            s.account = lease.account.clone();
        }
        cx.pool
            .report(&lease.account, out.failure.as_ref(), out.cost);
        // Live telemetry the account pool needs to stop routing BEFORE the provider says no.
        if let Some(snap) = out.rate_limit.clone() {
            cx.pool.observe_quota(&lease.account, snap);
        }
        // A cancelled node was killed by us: the classifier only sees SIGTERM and would retry.
        if cx.cancel.is_cancelled() {
            out.failure = Some(Failure::Cancelled {
                by: crate::model::core::CancelSource::User,
            });
        }

        record.usage = out.usage;
        record.cost = out.cost;
        record.exit = out.exit;
        record.summary = out.summary.clone();
        record.files = out.files.clone();
        if let Some(s) = out.session.clone() {
            record.session = Some(s);
        }
        record.stream_offset = out.stream_offset;
        record.unparsed_lines = out.unparsed_lines;
        record.ended_at = Some(time::OffsetDateTime::now_utc());
        record.state = match &out.failure {
            None => NodeState::Succeeded,
            Some(Failure::Cancelled { by }) => NodeState::Cancelled { by: *by },
            Some(f) => NodeState::Failed { failure: f.clone() },
        };
        if out.failure.is_none() {
            // The worktree is keyed by the logical node; the diff belongs to this attempt.
            let fwt = NodeWorktree {
                node: record.id,
                ..wt.clone()
            };
            record.work = cx
                .runner
                .finalize(&fwt, &task.title, spec.tier)
                .await
                .unwrap_or_default();
        }
        emit_for(
            cx,
            record.id,
            JournalEvent::NodeFinished {
                state: record.state.clone(),
                exit: record.exit,
                usage: record.usage,
                cost: record.cost,
                work: record.work.clone(),
                summary: record.summary.clone(),
                files: record.files.clone(),
                unparsed_lines: record.unparsed_lines,
            },
        );
        attempts.push(record.clone());

        match &out.failure {
            None => return settle(logical, attempts, out),
            Some(f) if f.rotates_account() => {
                excluded.insert(lease.account.clone());
                prev = Some(record.id);
                emit_for(
                    cx,
                    record.id,
                    JournalEvent::NodeRetry {
                        attempt,
                        reason: f.clone(),
                        rotate: true,
                    },
                );
                // A resume handle minted under another account does not exist here.
                spec.session = SessionPlan::New {
                    preassigned: cx.new_session_id(provider),
                };
                continue;
            }
            Some(f) if f.retries_same_account() => {
                prev = Some(record.id);
                emit_for(
                    cx,
                    record.id,
                    JournalEvent::NodeRetry {
                        attempt,
                        reason: f.clone(),
                        rotate: false,
                    },
                );
                // Resume the same session on the same account so the retry does not repay context.
                if let Some(h) = out.session.clone() {
                    spec.session = SessionPlan::Resume(h);
                }
                held = Some(lease);
                tokio::time::sleep(jitter(backoff)).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
            // Terminal: the TASK failed, not the infrastructure. Stop.
            Some(_) => return settle(logical, attempts, out),
        }
    }

    let mut out = NodeOutcome::failed(Failure::WorkerError {
        subtype: "attempts_exhausted".into(),
        detail: format!("{} attempts", cx.max_attempts),
    });
    out.logical = logical;
    out.attempts = attempts;
    out
}

fn settle(logical: NodeId, attempts: Vec<NodeRecord>, out: RunOutcome) -> NodeOutcome {
    let mut outcome = NodeOutcome::ok(out);
    outcome.logical = logical;
    outcome.attempts = attempts;
    outcome
}

fn cancelled(logical: NodeId, attempts: Vec<NodeRecord>) -> NodeOutcome {
    fail(
        logical,
        attempts,
        Failure::Cancelled {
            by: crate::model::core::CancelSource::User,
        },
    )
}

/// A worktree, a prompt file or a tier mapping that would not come up. Exit 3 is reserved for
/// a pool with nothing left in it, so this is never `NoCapacity`.
fn setup_failed(e: impl std::fmt::Display) -> Failure {
    Failure::WorkerError {
        subtype: "setup".into(),
        detail: format!("{e:#}"),
    }
}

fn fail(logical: NodeId, attempts: Vec<NodeRecord>, failure: Failure) -> NodeOutcome {
    let mut outcome = NodeOutcome::failed(failure);
    outcome.logical = logical;
    outcome.attempts = attempts;
    outcome
}

fn give_up(
    cx: &NodeCtx,
    logical: NodeId,
    attempts: Vec<NodeRecord>,
    provider: Provider,
    excluded: &HashSet<AccountId>,
) -> NodeOutcome {
    let cooling = cx
        .pool
        .snapshot()
        .into_iter()
        .filter(|(p, _, s)| {
            *p == provider
                && s.cooldown_until
                    .is_some_and(|t| t > time::OffsetDateTime::now_utc())
        })
        .count();
    let detail = crate::error::SwampError::NoAccountAvailable {
        provider,
        excluded: excluded.len(),
        cooling,
    }
    .to_string();
    fail(logical, attempts, Failure::NoCapacity { detail })
}

fn crashed_outcome(e: anyhow::Error) -> RunOutcome {
    RunOutcome {
        failure: Some(Failure::Crashed { signal: None }),
        exit: None,
        session: None,
        usage: Default::default(),
        cost: None,
        summary: Some(e.to_string()),
        files: Vec::new(),
        rate_limit: None,
        stream_offset: 0,
        unparsed_lines: 0,
        permission_denials: 0,
    }
}

fn session_handle(plan: &SessionPlan, account: &AccountId) -> Option<SessionHandle> {
    match plan {
        SessionPlan::Resume(h) => Some(h.clone()),
        SessionPlan::New { preassigned } => preassigned.as_ref().map(|id| SessionHandle {
            account: account.clone(),
            id: id.clone(),
            preassigned: true,
        }),
    }
}

fn write_prompt(
    dir: &Utf8PathBuf,
    prompt: &str,
    max_bytes: usize,
) -> anyhow::Result<(Utf8PathBuf, String)> {
    use sha2::{Digest, Sha256};
    anyhow::ensure!(
        prompt.len() <= max_bytes,
        "prompt is {} bytes, over limits.max_prompt_bytes = {max_bytes}",
        prompt.len()
    );
    std::fs::create_dir_all(dir)?;
    let path = dir.join("prompt.md");
    std::fs::write(&path, prompt)?;
    let sha = Sha256::digest(prompt.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((path, sha))
}

fn node_dir(cx: &NodeCtx, node: NodeId) -> Utf8PathBuf {
    cx.journal.paths.node_dir(node)
}

fn emit(cx: &NodeCtx, event: JournalEvent) {
    crate::dispatch::emit(&cx.journal, Some(cx.logical), event);
}

fn emit_for(cx: &NodeCtx, node: NodeId, event: JournalEvent) {
    crate::dispatch::emit(&cx.journal, Some(node), event);
}

fn jitter(d: Duration) -> Duration {
    let factor = rand::rng().random_range(0.5..1.0);
    d.mul_f64(factor)
}
