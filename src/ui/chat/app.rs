use crate::brain::BrainEvent;
use crate::config::schema::AccountCfg;
use crate::dispatch::account::AccountState;
use crate::ids::{NodeId, RunId};
use crate::journal::fold::RunView;
use crate::journal::record::JournalLine;
use crate::model::core::{AccountId, NodeState, Provider, Tier};
use crate::ui::chat::blocks::{Block, Ctx, ToolState, WelcomeInfo, bullet_first, tool_args};
use crate::ui::chat::input::{Editor, History};
use crate::ui::chat::markdown::MdStream;
use crate::ui::chat::theme::{Glyph, Role, Theme};
use crate::ui::chat::workers::{Batch, brain_children, expected_tasks};
use crate::ui::chat::{slash, spinner};
use crate::ui::{fmt, trace, watch};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Line;
use std::str::FromStr;
use std::time::Duration;
use time::OffsetDateTime;

/// Rule, input, rule, status: the floor under the live area.
pub const MIN_LIVE: u16 = 4;
const ARM: Duration = Duration::from_secs(2);
const NOTE: Duration = Duration::from_secs(3);
const DEFAULT_COLLAPSE: usize = 3;
const DIFF_COLLAPSE: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Working { since: OffsetDateTime },
    Interrupting,
}

pub enum Msg {
    Key(KeyEvent),
    Brain(BrainEvent),
    Journal(Vec<JournalLine>),
    Tick,
    Signal,
    Resize(u16, u16),
    /// The brain's channel closed, or the key stream ended: nothing more will arrive.
    Quit,
}

pub enum Effect {
    Commit(Vec<Line<'static>>),
    Send(String),
    Interrupt,
    CancelAll,
    Cancel(NodeId),
    /// `/trace` needs the event stream, which the board deliberately does not keep.
    Trace(Option<NodeId>),
    Clear,
    Quit(i32),
}

pub struct App {
    pub theme: Theme,
    pub width: u16,
    pub rows: u16,
    pub now: OffsetDateTime,
    pub tick: u64,
    pub run: RunId,
    pub brain: NodeId,
    pub view: RunView,
    pub pool: Vec<(Provider, AccountId, AccountState)>,
    /// `exec` and `max_concurrency` for `/usage`; the pool snapshot alone does not carry them.
    account_cfg: Vec<AccountCfg>,
    pub blocks: Vec<Block>,
    pub editor: Editor,
    pub history: History,
    pub phase: Phase,
    pub popup: Option<usize>,
    pub overlay: bool,
    pub pending_send: Option<String>,
    pub note: Option<(String, OffsetDateTime)>,
    pub show_thinking: bool,
    pub tier: Tier,
    pub collapse: usize,
    pub placeholder: String,
    pub turns: u32,
    pub out_tokens: u64,
    pub cost_usd: f64,
    pub exit_code: i32,
    esc_armed: Option<OffsetDateTime>,
    quit_armed: Option<OffsetDateTime>,
    resume_armed: Option<String>,
    est_bytes: u64,
    /// Every logical node a batch has ever taken. A committed board leaves the live area,
    /// and without this its nodes would be admitted a second time as a loose batch.
    admitted: std::collections::BTreeSet<NodeId>,
    last_collapsed: Option<Block>,
    seed: u64,
}

impl App {
    pub fn new(
        run: RunId,
        theme: Theme,
        welcome: WelcomeInfo,
        history: History,
        cfg: &crate::config::Config,
    ) -> App {
        let mut app = App {
            theme,
            width: 100,
            rows: 24,
            now: OffsetDateTime::now_utc(),
            tick: 0,
            run,
            brain: NodeId(run.0),
            view: RunView::default(),
            pool: Vec::new(),
            account_cfg: cfg.accounts.clone(),
            blocks: Vec::new(),
            editor: Editor::default(),
            history,
            phase: Phase::Idle,
            popup: None,
            overlay: false,
            pending_send: None,
            note: None,
            show_thinking: cfg.ui.show_thinking.unwrap_or(false),
            tier: cfg.dispatch.default_tier.unwrap_or(Tier::Mid),
            collapse: cfg.ui.collapse_lines.unwrap_or(DEFAULT_COLLAPSE).max(1),
            placeholder: "Try \"dispatch two workers to split the pagination work\"".to_owned(),
            turns: 0,
            out_tokens: 0,
            cost_usd: 0.0,
            exit_code: 0,
            esc_armed: None,
            quit_armed: None,
            resume_armed: None,
            est_bytes: 0,
            admitted: std::collections::BTreeSet::new(),
            last_collapsed: None,
            seed: spinner::seed_of(run),
        };
        app.blocks.push(Block::Welcome(welcome));
        app
    }

    pub fn ctx(&self, live: bool) -> Ctx<'_> {
        Ctx {
            width: self.width,
            theme: &self.theme,
            collapse: self.collapse,
            tick: self.tick,
            live,
        }
    }

    pub fn reduce(&mut self, msg: Msg) -> Vec<Effect> {
        let mut out = match msg {
            Msg::Key(k) => self.on_key(k),
            Msg::Brain(e) => self.on_brain(e),
            Msg::Journal(lines) => self.on_journal(&lines),
            Msg::Tick => self.on_tick(),
            Msg::Signal => vec![Effect::CancelAll, Effect::Quit(6)],
            Msg::Quit => vec![Effect::Quit(self.exit_code)],
            Msg::Resize(w, h) => {
                self.width = w.max(20);
                self.rows = h.max(MIN_LIVE);
                self.rewrap();
                Vec::new()
            }
        };
        out.extend(self.sweep());
        out
    }

    /// The very first block, committed before anything else reaches the screen.
    pub fn take_welcome(&mut self) -> Vec<Line<'static>> {
        if !matches!(self.blocks.first(), Some(Block::Welcome(_))) {
            return Vec::new();
        }
        let block = self.blocks.remove(0);
        block.render(&self.ctx(false))
    }

    pub fn set_pool(&mut self, pool: Vec<(Provider, AccountId, AccountState)>) {
        self.pool = pool;
    }

    /// The same liveness rule `swamp watch` uses: a dead worker stops spinning forever.
    pub fn mark_orphans(&mut self, alive: &dyn Fn(NodeId) -> bool) {
        self.view.mark_orphans(alive);
    }

    pub fn working(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    /// Whether anything on screen is animating; an idle chat must not wake 12 times a second.
    pub fn animating(&self) -> bool {
        self.working()
            || self.note.is_some()
            || self.esc_armed.is_some()
            || self.quit_armed.is_some()
            || self.batches().any(|b| b.running() > 0)
    }

    pub fn note(&mut self, text: impl Into<String>) {
        self.note = Some((text.into(), self.now + NOTE));
    }

    pub fn note_cancelled(&mut self, n: usize) -> Vec<Effect> {
        let head = format!("cancelled {n} {}", if n == 1 { "node" } else { "nodes" });
        vec![self.commit(Block::Notice {
            glyph: self.theme.g(Glyph::Cancelled).to_owned(),
            role: Role::Meta,
            head,
            body: Vec::new(),
        })]
    }

    // ------------------------------------------------------------ brain

    fn on_brain(&mut self, event: BrainEvent) -> Vec<Effect> {
        match event {
            BrainEvent::Ready { .. } => Vec::new(),
            BrainEvent::Text { delta } => {
                self.est_bytes += delta.len() as u64;
                self.stream(&delta, false)
            }
            BrainEvent::Thinking { delta } if self.show_thinking => self.stream(&delta, true),
            BrainEvent::Thinking { .. } => Vec::new(),
            BrainEvent::ToolCall { id, name, preview } => self.tool_call(id, name, preview),
            BrainEvent::ToolDone { id, ok, detail, .. } => self.tool_done(&id, ok, detail),
            BrainEvent::TurnDone { usage, cost } => {
                self.turns += 1;
                self.out_tokens = self.out_tokens.max(usage.output_tokens);
                self.est_bytes = 0;
                if let Some(c) = cost {
                    self.cost_usd += c.usd;
                }
                self.phase = Phase::Idle;
                let mut out = self.finish_streams();
                if let Some(text) = self.pending_send.take() {
                    self.phase = Phase::Working { since: self.now };
                    out.push(Effect::Send(text));
                }
                out
            }
            BrainEvent::Fatal { message } => {
                self.phase = Phase::Idle;
                self.exit_code = 1;
                let mut out = self.finish_streams();
                let head = format!(
                    "brain failed: {}",
                    fmt::truncate(&message, self.width.saturating_sub(16) as usize)
                );
                out.push(self.commit(Block::Notice {
                    glyph: self.theme.g(Glyph::Failed).to_owned(),
                    role: Role::Err,
                    head,
                    body: vec![format!(
                        "/accounts for the pool · swamp chat --resume {}",
                        self.run.short()
                    )],
                }));
                out
            }
        }
    }

    /// Assistant text commits line by line; only the incomplete trailing line stays live.
    fn stream(&mut self, delta: &str, thinking: bool) -> Vec<Effect> {
        let width = self.width;
        let theme = self.theme;
        let matches_kind = |b: &Block| match b {
            Block::Assistant { .. } => !thinking,
            Block::Thinking { .. } => thinking,
            _ => false,
        };
        if !self.blocks.last().is_some_and(matches_kind) {
            let md = MdStream::new(width, theme);
            self.blocks.push(if thinking {
                Block::Thinking { md, started: false }
            } else {
                Block::Assistant {
                    md,
                    started: false,
                    done: false,
                }
            });
        }
        let glyph = if thinking {
            theme.g(Glyph::Star)
        } else {
            theme.g(Glyph::Bullet)
        };
        let role = if thinking { Role::Meta } else { Role::Accent };
        let Some(last) = self.blocks.last_mut() else {
            return Vec::new();
        };
        let (md, started) = match last {
            Block::Assistant { md, started, .. } | Block::Thinking { md, started } => (md, started),
            _ => return Vec::new(),
        };
        let mut lines = md.push(delta);
        if lines.is_empty() {
            return Vec::new();
        }
        if !*started {
            bullet_first(&mut lines, &theme, glyph, role);
            *started = true;
        }
        vec![Effect::Commit(lines)]
    }

    /// Flushes a half-written stream into scrollback, so nothing is lost at a turn boundary.
    fn finish_streams(&mut self) -> Vec<Effect> {
        let theme = self.theme;
        let mut out = Vec::new();
        let mut keep = Vec::new();
        for mut block in std::mem::take(&mut self.blocks) {
            let (glyph, role) = match &block {
                Block::Assistant { .. } => (theme.g(Glyph::Bullet), Role::Accent),
                Block::Thinking { .. } => (theme.g(Glyph::Star), Role::Meta),
                _ => {
                    keep.push(block);
                    continue;
                }
            };
            let mut lines = match &mut block {
                Block::Assistant { md, started, .. } | Block::Thinking { md, started } => {
                    let lines = md.finish();
                    if !*started && !lines.is_empty() {
                        *started = true;
                        let mut lines = lines;
                        bullet_first(&mut lines, &theme, glyph, role);
                        lines
                    } else {
                        lines
                    }
                }
                _ => Vec::new(),
            };
            lines.push(Line::from(String::new()));
            out.push(Effect::Commit(lines));
        }
        self.blocks = keep;
        out
    }

    fn tool_call(&mut self, id: String, name: String, preview: String) -> Vec<Effect> {
        let mut out = self.finish_streams();
        if tool_args::short_name(&name) == "swamp_dispatch" {
            let mut batch = Batch::new(id, expected_tasks(&preview), self.now);
            batch.name = tool_args::short_name(&name);
            batch.preview = preview;
            self.blocks.push(Block::Dispatch(Box::new(batch)));
            return out;
        }
        self.blocks.push(Block::Tool {
            id,
            name,
            preview,
            state: ToolState::Running,
            result: Vec::new(),
            expanded: false,
        });
        out.extend(self.sweep());
        out
    }

    fn tool_done(&mut self, id: &str, ok: bool, detail: Option<String>) -> Vec<Effect> {
        let collapse_at = self.width.saturating_sub(6) as usize;
        for block in &mut self.blocks {
            match block {
                Block::Dispatch(batch) if batch.tool_id == id => {
                    batch.closed = true;
                    batch.ok = Some(ok);
                    return Vec::new();
                }
                Block::Tool {
                    id: tool,
                    state,
                    result,
                    ..
                } if tool == id => {
                    *state = if ok { ToolState::Ok } else { ToolState::Failed };
                    *result = body_lines(detail.as_deref(), collapse_at);
                    break;
                }
                _ => {}
            }
        }
        // A finished tool is scrollback the moment it settles.
        let Some(at) = self.blocks.iter().position(|b| match b {
            Block::Tool {
                id: tool, state, ..
            } => tool == id && *state != ToolState::Running,
            _ => false,
        }) else {
            return Vec::new();
        };
        let block = self.blocks.remove(at);
        vec![self.commit(block)]
    }

    // ------------------------------------------------------------ journal

    fn on_journal(&mut self, lines: &[JournalLine]) -> Vec<Effect> {
        for l in lines {
            self.view.apply(l);
        }
        self.admit();
        Vec::new()
    }

    /// Any brain child in the fold joins the open batch; a node with no call behind it joins
    /// the loose batch. Dispatch calls from one brain are serial, so this is exact.
    fn admit(&mut self) {
        let children = brain_children(&self.view, self.brain);
        let fresh: Vec<NodeId> = children
            .into_iter()
            .filter(|c| !self.admitted.contains(c))
            .collect();
        if !fresh.is_empty() {
            self.admitted.extend(fresh.iter().copied());
            let open = self.blocks.iter_mut().rev().find_map(|b| match b {
                Block::Dispatch(batch) if !batch.closed => Some(batch),
                _ => None,
            });
            match open {
                Some(batch) => batch.owned.extend(fresh),
                None => {
                    let loose = self.blocks.iter_mut().rev().find_map(|b| match b {
                        Block::Dispatch(batch) if batch.loose && !batch.done() => Some(batch),
                        _ => None,
                    });
                    match loose {
                        Some(batch) => batch.owned.extend(fresh),
                        None => {
                            let mut batch = Batch::loose(self.now);
                            batch.owned.extend(fresh);
                            self.blocks.push(Block::Dispatch(Box::new(batch)));
                        }
                    }
                }
            }
        }
        let (view, now) = (&self.view, self.now);
        for block in &mut self.blocks {
            if let Block::Dispatch(batch) = block {
                batch.refresh(view, now);
            }
        }
    }

    fn batches(&self) -> impl Iterator<Item = &Batch> {
        self.blocks.iter().filter_map(|b| match b {
            Block::Dispatch(batch) => Some(batch.as_ref()),
            _ => None,
        })
    }

    /// Commits every live block that has nothing left to say.
    fn sweep(&mut self) -> Vec<Effect> {
        let Some(at) = self.blocks.iter().position(|b| match b {
            Block::Dispatch(batch) => batch.done(),
            _ => false,
        }) else {
            return Vec::new();
        };
        let block = self.blocks.remove(at);
        let mut out = vec![self.commit(block)];
        out.extend(self.sweep());
        out
    }

    fn on_tick(&mut self) -> Vec<Effect> {
        self.tick = self.tick.wrapping_add(1);
        if self.note.as_ref().is_some_and(|(_, at)| *at <= self.now) {
            self.note = None;
        }
        if self.esc_armed.is_some_and(|at| at <= self.now) {
            self.esc_armed = None;
        }
        if self.quit_armed.is_some_and(|at| at <= self.now) {
            self.quit_armed = None;
        }
        let (view, now) = (&self.view, self.now);
        for block in &mut self.blocks {
            if let Block::Dispatch(batch) = block {
                batch.refresh(view, now);
            }
        }
        Vec::new()
    }

    // ------------------------------------------------------------ keys

    pub fn on_key(&mut self, k: KeyEvent) -> Vec<Effect> {
        if k.kind != KeyEventKind::Press {
            return Vec::new();
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        if self.overlay && !matches!(k.code, KeyCode::Esc) {
            self.overlay = false;
            return Vec::new();
        }
        match k.code {
            KeyCode::Char('c') if ctrl => self.on_ctrl_c(),
            KeyCode::Char('d') if ctrl && self.editor.is_empty() => vec![Effect::Quit(0)],
            KeyCode::Char('l') if ctrl => vec![Effect::Clear],
            KeyCode::Char('o') if ctrl => self.expand(),
            KeyCode::Char('a') if ctrl => {
                self.editor.home();
                Vec::new()
            }
            KeyCode::Char('e') if ctrl => {
                self.editor.end();
                Vec::new()
            }
            KeyCode::Char('k') if ctrl => {
                self.editor.kill_to_end();
                Vec::new()
            }
            KeyCode::Char('u') if ctrl => {
                self.editor.kill_to_start();
                Vec::new()
            }
            KeyCode::Char('w') if ctrl => {
                self.editor.kill_word();
                Vec::new()
            }
            KeyCode::Enter if self.popup.is_some() && !slash::runnable(self.editor.text()) => {
                self.accept_popup();
                Vec::new()
            }
            KeyCode::Enter if alt || shift => {
                self.editor.insert('\n');
                Vec::new()
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Esc => self.on_esc(),
            KeyCode::Tab => {
                self.complete();
                Vec::new()
            }
            KeyCode::Backspace if alt => {
                self.editor.kill_word();
                self.sync_popup();
                Vec::new()
            }
            KeyCode::Backspace => {
                self.editor.backspace();
                self.sync_popup();
                Vec::new()
            }
            KeyCode::Delete => {
                self.editor.delete();
                Vec::new()
            }
            KeyCode::Left if alt => {
                self.editor.word_left();
                Vec::new()
            }
            KeyCode::Right if alt => {
                self.editor.word_right();
                Vec::new()
            }
            KeyCode::Left => {
                self.editor.left();
                Vec::new()
            }
            KeyCode::Right => {
                self.editor.right();
                Vec::new()
            }
            KeyCode::Home => {
                self.editor.home();
                Vec::new()
            }
            KeyCode::End => {
                self.editor.end();
                Vec::new()
            }
            KeyCode::Up => {
                self.move_up();
                Vec::new()
            }
            KeyCode::Down => {
                self.move_down();
                Vec::new()
            }
            KeyCode::Char('?') if self.editor.is_empty() => {
                self.overlay = true;
                Vec::new()
            }
            KeyCode::Char('/') if self.editor.is_empty() => {
                self.editor.insert('/');
                self.popup = Some(0);
                Vec::new()
            }
            KeyCode::Char(c) => {
                self.editor.insert(c);
                self.sync_popup();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn on_ctrl_c(&mut self) -> Vec<Effect> {
        if !self.editor.is_empty() {
            self.editor.clear();
            self.quit_armed = None;
            self.popup = None;
            return Vec::new();
        }
        if self.quit_armed.is_some() {
            let code = if self.working() { 6 } else { 0 };
            return vec![Effect::Interrupt, Effect::CancelAll, Effect::Quit(code)];
        }
        self.quit_armed = Some(self.now + ARM);
        self.note("Press ctrl+c again to exit");
        Vec::new()
    }

    fn on_esc(&mut self) -> Vec<Effect> {
        if self.popup.is_some() || self.overlay {
            self.popup = None;
            self.overlay = false;
            return Vec::new();
        }
        if self.esc_armed.is_some() {
            self.esc_armed = None;
            return vec![Effect::CancelAll];
        }
        if self.working() {
            self.phase = Phase::Interrupting;
            self.esc_armed = Some(self.now + ARM);
            let running: usize = self.batches().map(Batch::running).sum();
            let head = if running > 0 {
                format!(
                    "interrupted · {running} workers still running (esc again within 2s to cancel them)"
                )
            } else {
                "interrupted".to_owned()
            };
            let notice = Block::Notice {
                glyph: self.theme.g(Glyph::Cancelled).to_owned(),
                role: Role::Meta,
                head,
                body: Vec::new(),
            };
            let commit = self.commit(notice);
            self.phase = Phase::Idle;
            return vec![Effect::Interrupt, commit];
        }
        if !self.editor.is_empty() {
            self.editor.clear();
        }
        Vec::new()
    }

    fn submit(&mut self) -> Vec<Effect> {
        if self.editor.text().ends_with('\\') {
            self.editor.backspace();
            self.editor.insert('\n');
            return Vec::new();
        }
        let text = self.editor.text().trim().to_owned();
        if text.is_empty() {
            return Vec::new();
        }
        self.editor.clear();
        self.popup = None;
        self.history.push(&text);
        self.history.reset();
        let bar = self.commit(Block::User(text.clone()));
        if let Some(command) = text.strip_prefix('/') {
            let mut out = vec![bar];
            out.extend(self.slash(command));
            return out;
        }
        if self.working() {
            self.pending_send = Some(text);
            return vec![bar];
        }
        self.phase = Phase::Working { since: self.now };
        self.est_bytes = 0;
        self.out_tokens = 0;
        vec![bar, Effect::Send(text)]
    }

    fn move_up(&mut self) {
        if let Some(sel) = self.popup {
            self.popup = Some(sel.saturating_sub(1));
            return;
        }
        if self.editor.up() {
            return;
        }
        if let Some(entry) = self.history.prev(self.editor.text()) {
            self.editor.set(&entry);
        }
    }

    fn move_down(&mut self) {
        if let Some(sel) = self.popup {
            let last = slash::filter(self.editor.text())
                .len()
                .min(slash::MAX_ROWS)
                .saturating_sub(1);
            self.popup = Some((sel + 1).min(last));
            return;
        }
        if self.editor.down() {
            return;
        }
        if let Some(entry) = self.history.forward() {
            self.editor.set(&entry);
        }
    }

    fn sync_popup(&mut self) {
        if self.editor.text().starts_with('/') {
            if self.popup.is_none() {
                self.popup = Some(0);
            }
        } else {
            self.popup = None;
        }
    }

    fn complete(&mut self) {
        if self.popup.is_none() {
            if self.editor.text().starts_with('/') {
                self.popup = Some(0);
            }
            return;
        }
        let prefix = slash::common_prefix(self.editor.text());
        if prefix.len() > self.editor.text().len() {
            self.editor.set(&prefix);
            return;
        }
        self.accept_popup();
    }

    fn accept_popup(&mut self) {
        let Some(sel) = self.popup else {
            return;
        };
        let cands = slash::filter(self.editor.text());
        if let Some(cmd) = cands.get(sel) {
            self.editor.set(cmd.name);
        }
        self.popup = None;
    }

    /// ctrl+o: live blocks toggle in place, committed ones get a fresh expanded block,
    /// because scrollback is immutable.
    fn expand(&mut self) -> Vec<Effect> {
        let collapse = self.collapse;
        if let Some(block) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|b| b.collapsible(collapse))
        {
            block.toggle();
            return Vec::new();
        }
        let Some(mut block) = self.last_collapsed.take() else {
            self.note("nothing to expand");
            return Vec::new();
        };
        block.toggle();
        let name = match &block {
            Block::Tool { name, .. } => tool_args::short_name(name),
            _ => "workers".to_owned(),
        };
        let mut lines = vec![Line::from(self.theme.span(
            format!("  {}  expanded: {name}", self.theme.g(Glyph::Connector)),
            Role::Meta,
        ))];
        lines.extend(block.render(&self.ctx(false)));
        vec![Effect::Commit(lines)]
    }

    fn commit(&mut self, block: Block) -> Effect {
        let lines = block.render(&self.ctx(false));
        if block.collapsible(self.collapse) {
            self.last_collapsed = Some(block);
        }
        Effect::Commit(lines)
    }

    /// Width changes re-wrap the live tail; scrollback is frozen text and stays as it is.
    pub fn set_width(&mut self, width: u16) {
        self.width = width.max(20);
        self.rewrap();
    }

    fn rewrap(&mut self) {
        let width = self.width;
        for block in &mut self.blocks {
            match block {
                Block::Assistant { md, .. } | Block::Thinking { md, .. } => md.set_width(width),
                _ => {}
            }
        }
    }

    // ------------------------------------------------------------ slash

    fn slash(&mut self, command: &str) -> Vec<Effect> {
        let mut parts = command.split_whitespace();
        let name = parts.next().unwrap_or_default().to_ascii_lowercase();
        let arg = parts.next().map(str::to_owned);
        match name.as_str() {
            "help" | "?" => vec![self.output("commands", slash::help_body())],
            "status" => {
                let body = trace::render(&self.view, &trace::TraceOpts::default());
                vec![self.output("", lines_of(&body))]
            }
            "trace" => {
                let node = arg.as_deref().and_then(|a| self.find_node(a));
                vec![Effect::Trace(node)]
            }
            "accounts" => {
                let body = self.accounts_body();
                vec![self.output("", body)]
            }
            "usage" => vec![self.usage_output(arg.as_deref() == Some("--json"))],
            "cost" => {
                let body = self.cost_body();
                vec![self.output("", body)]
            }
            "tier" => self.set_tier(arg.as_deref()),
            "cancel" => self.cancel(arg.as_deref()),
            "diff" => self.diff(arg.as_deref()),
            "thinking" => {
                self.show_thinking = match arg.as_deref() {
                    Some("on") => true,
                    Some("off") => false,
                    _ => !self.show_thinking,
                };
                let state = if self.show_thinking { "on" } else { "off" };
                vec![self.output("", vec![format!("thinking: {state}")])]
            }
            "clear" => {
                self.blocks.clear();
                vec![
                    Effect::Clear,
                    self.output(
                        "",
                        vec![
                            "screen cleared; the brain still remembers the conversation".to_owned(),
                        ],
                    ),
                ]
            }
            "resume" => {
                let spec = arg.unwrap_or_else(|| self.run.short());
                let line = format!("swamp chat --resume {spec}");
                if self.resume_armed.as_deref() == Some(spec.as_str()) {
                    return vec![Effect::Quit(0)];
                }
                self.resume_armed = Some(spec);
                vec![self.output("", vec![line, "/resume again to leave now".to_owned()])]
            }
            "quit" | "exit" | "q" => vec![Effect::Quit(0)],
            other => {
                let head = format!("unknown command /{other}; {}", slash::did_you_mean(other));
                vec![self.commit(Block::Notice {
                    glyph: self.theme.g(Glyph::Failed).to_owned(),
                    role: Role::Err,
                    head,
                    body: Vec::new(),
                })]
            }
        }
    }

    /// `/trace` renders from a second fold, the only slash output the reducer cannot build.
    pub fn trace_output(&mut self, text: &str) -> Vec<Effect> {
        let body = lines_of(text);
        vec![self.output("", body)]
    }

    fn output(&mut self, title: &str, body: Vec<String>) -> Effect {
        self.commit(Block::Slash {
            title: title.to_owned(),
            body,
        })
    }

    fn set_tier(&mut self, arg: Option<&str>) -> Vec<Effect> {
        let Some(arg) = arg else {
            return vec![self.output("", vec![format!("dispatch tier: {}", self.tier)])];
        };
        match Tier::from_str(arg) {
            Ok(tier) => {
                let body = vec![format!("dispatch tier: {} -> {tier}", self.tier)];
                self.tier = tier;
                vec![self.output("", body)]
            }
            Err(_) => {
                self.note("/tier takes low, mid or high");
                Vec::new()
            }
        }
    }

    fn cancel(&mut self, arg: Option<&str>) -> Vec<Effect> {
        match arg {
            Some("all") => vec![Effect::CancelAll],
            Some(spec) => match self.find_node(spec) {
                Some(node) => vec![Effect::Cancel(node)],
                None => {
                    self.note(format!("no node matches {spec} · try /status"));
                    Vec::new()
                }
            },
            None => {
                self.note("/cancel needs a node id or all · try /status");
                Vec::new()
            }
        }
    }

    fn diff(&mut self, arg: Option<&str>) -> Vec<Effect> {
        let Some(spec) = arg else {
            self.note("/diff needs a node id · try /status");
            return Vec::new();
        };
        let Some(node) = self.find_node(spec).and_then(|id| self.view.nodes.get(&id)) else {
            self.note(format!("no node matches {spec} · try /status"));
            return Vec::new();
        };
        let mut body: Vec<String> = node
            .files
            .iter()
            .map(|f| format!(" {}  +{} -{}", f.path, f.added, f.removed))
            .collect();
        if let Some(w) = &node.work {
            body.push(format!(
                " {} files changed, +{} -{}",
                node.files.len(),
                w.insertions,
                w.deletions
            ));
        }
        if body.is_empty() {
            body.push(format!("no patch recorded for {spec}"));
        }
        let hidden = body.len().saturating_sub(DIFF_COLLAPSE);
        if hidden > 0 {
            body.truncate(DIFF_COLLAPSE);
            body.push(format!("… +{hidden} lines (ctrl+o to expand)"));
        }
        vec![self.output("", body)]
    }

    fn accounts_body(&self) -> Vec<String> {
        self.pool
            .iter()
            .map(|(provider, id, state)| {
                let util = state.quota.as_ref().map_or(0.0, |q| q.worst_utilization());
                let cooldown = state
                    .cooldown_until
                    .filter(|t| *t > self.now)
                    .map(|t| format!("  until {}", fmt::clock_hm(t)))
                    .unwrap_or_default();
                format!(
                    "{provider}/{:<10} {:<11} {} {:>3}%  {} inflight  {} nodes  ~${:.2}{cooldown}",
                    id.0,
                    watch::health_word(state.health),
                    watch::gauge_bar(util),
                    (util * 100.0).round() as i64,
                    state.inflight,
                    state.lifetime_nodes,
                    state.lifetime_cost_usd,
                )
            })
            .collect()
    }

    /// Shares `ui::usage::render` byte-for-byte with `swamp usage`; only `--json` goes through
    /// `Block::Slash` since a table needs its own colouring, not the block's flat `meta`.
    fn usage_output(&mut self, json: bool) -> Effect {
        let stale: Vec<(AccountId, AccountState)> = Vec::new();
        let rows = crate::ui::usage::rows_from(&self.account_cfg, &self.pool, &stale);
        if json {
            let text =
                serde_json::to_string_pretty(&crate::ui::usage::json(&rows)).unwrap_or_default();
            let mut body: Vec<String> = vec!["```json".to_owned()];
            body.extend(text.lines().map(str::to_owned));
            body.push("```".to_owned());
            return self.output("", body);
        }
        Effect::Commit(crate::ui::usage::render(&rows, self.width, &self.theme))
    }

    fn cost_body(&self) -> Vec<String> {
        let t = self.view.totals();
        let mut out = vec![format!(
            "in {}  out {}  cache-read {}  cache-write {}",
            fmt::tokens(t.usage.input_tokens),
            fmt::tokens(t.usage.output_tokens),
            fmt::tokens(t.usage.cached_input_tokens),
            fmt::tokens(t.usage.cache_write_tokens),
        )];
        out.push(format!(
            "cost ~${:.2}{}",
            t.cost_usd,
            if t.cost_complete { "" } else { "+" }
        ));
        for (provider, id, state) in &self.pool {
            out.push(format!(
                "  {provider}/{:<10} {} nodes  ~${:.2}",
                id.0, state.lifetime_nodes, state.lifetime_cost_usd
            ));
        }
        for tier in [Tier::Low, Tier::Mid, Tier::High] {
            let usd: f64 = self
                .view
                .nodes
                .values()
                .filter(|n| n.tier == tier)
                .filter_map(|n| n.cost.map(|c| c.usd))
                .sum();
            let nodes = self.view.nodes.values().filter(|n| n.tier == tier).count();
            if nodes > 0 {
                out.push(format!("  [{tier:<4}] {nodes} nodes  ~${usd:.2}"));
            }
        }
        let unknown = self
            .view
            .nodes
            .values()
            .filter(|n| n.cost.is_none())
            .count();
        if unknown > 0 {
            let plural = if unknown == 1 { "node" } else { "nodes" };
            out.push(format!("({unknown} {plural} reported no cost data)"));
        }
        out
    }

    /// A short id, a full id or a prefix, against the nodes this run folded.
    fn find_node(&self, spec: &str) -> Option<NodeId> {
        let needle = spec.trim().trim_start_matches("nd_").to_ascii_lowercase();
        if needle.is_empty() {
            return None;
        }
        self.view
            .nodes
            .keys()
            .find(|id| {
                id.short() == needle || id.0.to_string().to_ascii_lowercase().starts_with(&needle)
            })
            .copied()
    }

    // ------------------------------------------------------------ chrome

    /// `✳ Pondering… (esc to interrupt · 12s · ↓ 1.2k tokens)`
    pub fn working_line(&self) -> Option<Line<'static>> {
        let Phase::Working { since } = self.phase else {
            return None;
        };
        let t = &self.theme;
        let elapsed: Duration = (self.now - since).try_into().unwrap_or(Duration::ZERO);
        let running: usize = self.batches().map(Batch::running).sum();
        let verb = spinner::verb(self.seed, elapsed, running > 0);
        let tail = if running > 0 {
            format!("{running} workers running")
        } else {
            format!(
                "{} {} tokens",
                t.g(Glyph::TokenArrow),
                fmt::tokens(self.out_tokens.max(self.est_bytes / 4))
            )
        };
        Some(Line::from(vec![
            t.span(
                format!("{} ", spinner::brain_frame(t, self.tick)),
                Role::Accent,
            ),
            t.span(format!("{verb}… "), Role::Accent),
            t.span(
                format!("(esc to interrupt · {} · {tail})", fmt::duration(elapsed)),
                Role::Meta,
            ),
        ]))
    }

    /// Three zones on one row; the centre goes first when the terminal narrows.
    pub fn status_line(&self) -> Line<'static> {
        let t = &self.theme;
        let width = self.width as usize;
        let left = match &self.note {
            Some((text, _)) => format!("  {text}"),
            None => "  ? for shortcuts".to_owned(),
        };
        let left = if self.popup.is_some() {
            "  tab completes · ↑↓ chooses · esc closes".to_owned()
        } else {
            left
        };
        let running: usize = self.batches().map(Batch::running).sum();
        let centre = format!(
            "{} dispatch {}{}",
            t.g(Glyph::Mode),
            self.tier,
            if running > 0 {
                format!(" · {running} running")
            } else {
                String::new()
            }
        );
        let state = if self.pending_send.is_some() {
            "1 queued".to_owned()
        } else if running > 0 {
            format!("{running} running")
        } else if self.working() {
            format!(
                "{} turn{}",
                self.turns + 1,
                if self.turns == 0 { "" } else { "s" }
            )
        } else if self.turns > 0 {
            format!(
                "{} turn{}",
                self.turns,
                if self.turns == 1 { "" } else { "s" }
            )
        } else {
            "idle".to_owned()
        };
        let total = self.cost_usd + self.view.cost_usd;
        let mut right = format!("run {} · {state} · ~${total:.2}", self.run.short());
        if width < 80 {
            right = format!("run {} · {state}", self.run.short());
        }
        if width < 50 {
            right = format!("run {}", self.run.short());
        }
        let centre_room = width.saturating_sub(left.len() + right.len() + 4);
        let mut text = left.clone();
        if width >= 80 && centre_room >= centre.chars().count() {
            let gap = (width - left.len() - right.len() - centre.chars().count()) / 2;
            text.push_str(&" ".repeat(gap));
            text.push_str(&centre);
        }
        let pad = width.saturating_sub(text.chars().count() + right.chars().count() + 2);
        text.push_str(&" ".repeat(pad));
        text.push_str(&right);
        Line::from(t.span(fmt::truncate(&text, width), Role::Meta))
    }
}

/// A tool result body: sanitized, truncated, never wrapped.
fn body_lines(detail: Option<&str>, width: usize) -> Vec<String> {
    let Some(detail) = detail else {
        return Vec::new();
    };
    detail
        .lines()
        .map(|l| fmt::truncate(l, width))
        .filter(|l| !l.is_empty())
        .collect()
}

fn lines_of(text: &str) -> Vec<String> {
    text.lines().map(str::to_owned).collect()
}

/// Whether a node is still one this board should animate.
pub fn is_running(state: &NodeState) -> bool {
    matches!(state, NodeState::Running { .. })
}
