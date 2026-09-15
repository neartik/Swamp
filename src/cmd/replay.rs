use crate::cli::ReplayArgs;
use crate::cmd::Ctx;
use crate::journal::fold::RunView;
use crate::journal::paths::RunPaths;
use crate::journal::record::{JournalEvent, JournalLine, SCHEMA_VERSION};
use crate::journal::writer::{FsyncPolicy, Writer};
use crate::model::core::{NodeState, Usage, WorkspaceRef};
use crate::model::node::{NodeRecord, WorkResultRef};
use crate::ui::trace::{TraceOpts, render};
use crate::worker::adapter::{ExitContext, ParseState};
use camino::Utf8PathBuf;
use time::OffsetDateTime;

/// Re-render, or re-derive, a recorded run.
pub async fn run(ctx: &Ctx, args: &ReplayArgs) -> anyhow::Result<i32> {
    let paths = ctx.run_paths(Some(&args.run))?;
    if !args.reparse {
        let view = ctx.view(&paths, true)?;
        ctx.out(&render(
            &view,
            &TraceOpts {
                events: true,
                json: ctx.json,
                ..TraceOpts::default()
            },
        ));
        return Ok(0);
    }

    let records = identities(ctx, &paths)?;
    anyhow::ensure!(
        !records.is_empty(),
        "run {} has no nodes to reparse: neither a journal nor a node result.json survived",
        paths.run
    );
    let count = rewrite(ctx, &paths, records).await?;
    println!(
        "reparsed {count} nodes of run {} with the current adapters",
        paths.run
    );
    let view = ctx.view(&paths, false)?;
    ctx.out(&render(&view, &TraceOpts::default()));
    Ok(0)
}

/// Node identity survives in the journal, and when that is gone, in each node's result.json.
fn identities(ctx: &Ctx, paths: &RunPaths) -> anyhow::Result<Vec<NodeRecord>> {
    if let Ok(view) = RunView::load(&paths.dir, false)
        && !view.nodes.is_empty()
    {
        return Ok(view.nodes.values().cloned().collect());
    }
    let mut out = Vec::new();
    let dir = paths.dir.join("nodes");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let Ok(path) = Utf8PathBuf::from_path_buf(entry.path().join("result.json")) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(result) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if let Some(record) = from_result(ctx, paths, &result) {
            out.push(record);
        }
    }
    Ok(out)
}

/// `result.json` is written by the cmd layer for exactly this case, so a node keeps its
/// identity even when the journal is gone.
fn from_result(ctx: &Ctx, paths: &RunPaths, r: &serde_json::Value) -> Option<NodeRecord> {
    use std::str::FromStr;
    let id = crate::ids::NodeId::from_str(r.get("node")?.as_str()?).ok()?;
    let provider = serde_json::from_value(r.get("provider")?.clone()).ok()?;
    let tier = serde_json::from_value(r.get("tier")?.clone()).unwrap_or(crate::model::core::Tier::Mid);
    let files = r
        .get("files")
        .and_then(|f| serde_json::from_value(f.clone()).ok())
        .unwrap_or_default();
    let prompt = paths.prompt(id);
    let prompt_sha256 = std::fs::read(&prompt)
        .map(|b| {
            use sha2::{Digest, Sha256};
            Sha256::digest(&b)
                .iter()
                .map(|x| format!("{x:02x}"))
                .collect()
        })
        .unwrap_or_default();
    let text = |key: &str| r.get(key).and_then(|v| v.as_str()).map(str::to_owned);
    let count = |key: &str| r.get(key).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let work = text("branch")
        .zip(text("patch"))
        .map(|(branch, patch)| WorkResultRef {
            head: String::new(),
            branch,
            patch: Utf8PathBuf::from(patch),
            insertions: count("insertions"),
            deletions: count("deletions"),
            empty: count("insertions") + count("deletions") == 0,
        });
    Some(NodeRecord {
        id,
        run_id: paths.run,
        parent: None,
        logical: id,
        attempt: count("attempts").max(1),
        retry_of: None,
        kind: crate::model::core::NodeKind::Worker,
        title: text("title").unwrap_or_default(),
        prompt_path: prompt,
        prompt_sha256,
        provider,
        account: text("account").map(crate::model::core::AccountId),
        exec: None,
        argv: Vec::new(),
        model: text("model"),
        tier,
        workspace: WorkspaceRef::ReadOnly {
            path: ctx.paths.repo.clone(),
        },
        session: None,
        state: NodeState::Queued,
        created_at: OffsetDateTime::now_utc(),
        started_at: None,
        ended_at: None,
        usage: Usage::default(),
        cost: None,
        exit: None,
        files,
        work,
        summary: text("summary"),
        stream_offset: 0,
        unparsed_lines: 0,
    })
}

/// The real answer to vendor schema drift: fix the adapter, then recover history from the
/// raw streams instead of losing it.
async fn rewrite(ctx: &Ctx, paths: &RunPaths, records: Vec<NodeRecord>) -> anyhow::Result<u32> {
    let journal = paths.journal();
    if journal.is_file() {
        std::fs::rename(&journal, journal.with_extension("jsonl.prev"))?;
    }
    let mut writer = Writer::open(&journal, FsyncPolicy::Always).await?;
    let mut seq = 0u64;

    let header = line(
        &mut seq,
        paths.run,
        None,
        JournalEvent::RunStarted {
            swamp_version: crate::VERSION.to_owned(),
            schema: SCHEMA_VERSION,
            argv: vec!["swamp".into(), "replay".into(), "--reparse".into()],
            cwd: ctx.paths.repo.clone(),
            repo: Some(ctx.paths.repo.clone()),
            base: None,
            config_sha256: ctx.cfg.sha256(),
            task: None,
        },
    );
    writer.append(&header).await?;

    let mut count = 0;
    let mut totals = Usage::default();
    let mut cost_usd = 0.0;
    for mut record in records {
        let adapter = crate::worker::adapter::adapter_for(record.provider);
        let patterns = ctx.cfg.failure_patterns(record.provider)?;
        let mut st = ParseState::default();
        let mut events = Vec::new();
        let raw = std::fs::read_to_string(paths.stream(record.id)).unwrap_or_default();
        let mut offset = 0u64;
        for raw_line in raw.split_inclusive('\n') {
            offset += raw_line.len() as u64;
            let out = adapter.parse_line(raw_line.trim_end_matches(['\n', '\r']), &mut st);
            if out.noise {
                st.unparsed += 1;
            }
            for e in out.events {
                events.push((offset, e));
            }
        }
        let failure = adapter.classify(&ExitContext {
            exit: record.exit,
            state: &st,
            patterns: &patterns,
            deadline_hit: false,
        });

        record.usage = st.usage;
        record.cost = st
            .last_final
            .as_ref()
            .and_then(|f| f.cost)
            .or_else(|| ctx.cfg.estimate_cost(record.model.as_deref().unwrap_or(""), &st.usage));
        record.stream_offset = offset;
        record.unparsed_lines = st.unparsed;
        record.summary = st
            .last_final
            .as_ref()
            .and_then(|f| f.text.clone())
            .or(record.summary.clone());
        if !st.files.is_empty() && record.files.is_empty() {
            record.files = st.files.clone();
        }
        record.state = match &failure {
            None => NodeState::Succeeded,
            Some(f) => NodeState::Failed { failure: f.clone() },
        };
        totals.absorb(&record.usage);
        cost_usd += record.cost.map_or(0.0, |c| c.usd);

        let node = Some(record.id);
        let spawned = line(
            &mut seq,
            paths.run,
            node,
            JournalEvent::NodeSpawned {
                node: Box::new(record.clone()),
            },
        );
        writer.append(&spawned).await?;
        for (offset, event) in events {
            let l = line(
                &mut seq,
                paths.run,
                node,
                JournalEvent::NodeEvent { offset, event },
            );
            writer.append(&l).await?;
        }
        let usage = line(
            &mut seq,
            paths.run,
            node,
            JournalEvent::NodeUsage {
                usage: record.usage,
                cost: record.cost,
            },
        );
        writer.append(&usage).await?;
        if !record.files.is_empty() {
            let files = line(
                &mut seq,
                paths.run,
                node,
                JournalEvent::NodeFiles {
                    files: record.files.clone(),
                },
            );
            writer.append(&files).await?;
        }
        let finished = line(
            &mut seq,
            paths.run,
            node,
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
        writer.append(&finished).await?;
        count += 1;
    }

    let done = line(
        &mut seq,
        paths.run,
        None,
        JournalEvent::RunFinished {
            state: NodeState::Succeeded,
            nodes: count,
            usage: totals,
            cost_usd: Some(cost_usd),
        },
    );
    writer.append(&done).await?;
    writer.sync().await?;
    Ok(count)
}

fn line(
    seq: &mut u64,
    run: crate::ids::RunId,
    node: Option<crate::ids::NodeId>,
    event: JournalEvent,
) -> JournalLine {
    let l = JournalLine {
        seq: *seq,
        at: OffsetDateTime::now_utc(),
        run,
        node,
        event,
    };
    *seq += 1;
    l
}
