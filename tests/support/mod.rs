//! The end-to-end harness: a throwaway repo, a throwaway HOME, a config, and a PATH of fake
//! CLIs. Every test here drives the real `swamp` binary; nothing is stubbed in process.
#![allow(dead_code)]

#[path = "scenario.rs"]
pub mod scenario;

pub use scenario::{Invocation, Scenario};

use camino::{Utf8Path, Utf8PathBuf};
use std::collections::BTreeMap;
use swamp::ids::RunId;
use swamp::journal::fold::RunView;
use swamp::journal::paths::{Paths, RunPaths};

/// Tier names the harness configures. Not model ids: the fakes never look at them.
pub const HIGH: &str = "fake-high";
pub const MID: &str = "fake-mid";
pub const LOW: &str = "fake-low";

#[derive(Debug, Clone)]
pub struct AccountSpec {
    pub id: String,
    pub exec: String,
    pub provider: &'static str,
    /// The per-subscription config dir the wrapper would export.
    pub config_dir: Utf8PathBuf,
    pub max_concurrency: usize,
    pub weight: u32,
}

pub struct Harness {
    _tmp: tempfile::TempDir,
    pub root: Utf8PathBuf,
    pub repo: Utf8PathBuf,
    pub home: Utf8PathBuf,
    /// Holds the wrapper executables, their scenario files and their invocation logs.
    pub bin: Utf8PathBuf,
    pub config_dir: Utf8PathBuf,
    pub accounts: Vec<AccountSpec>,
    scenarios: BTreeMap<String, Scenario>,
    extra_toml: String,
}

impl Harness {
    pub fn new() -> Harness {
        let tmp = tempfile::Builder::new()
            .prefix("swamp-e2e-")
            .tempdir_in(short_tmp())
            .expect("tempdir");
        // Canonicalized: macOS hands out /var, git reports /private/var.
        let root = std::fs::canonicalize(tmp.path()).expect("canonicalize");
        let root = Utf8PathBuf::from_path_buf(root).expect("utf8 tempdir");

        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        init_repo(&repo);

        let harness = Harness {
            _tmp: tmp,
            repo,
            home: root.join("home"),
            bin: root.join("bin"),
            config_dir: root.join("config"),
            root,
            accounts: Vec::new(),
            scenarios: BTreeMap::new(),
            extra_toml: String::new(),
        };
        for dir in [&harness.home, &harness.bin, &harness.config_dir] {
            std::fs::create_dir_all(dir).expect("harness dir");
        }
        harness.with_accounts(1, 0)
    }

    /// One wrapper executable per subscription, each with its own config dir.
    pub fn with_accounts(mut self, anthropic: usize, openai: usize) -> Harness {
        self.accounts.clear();
        for (i, id) in ["main", "alt", "third", "fourth"]
            .iter()
            .take(anthropic)
            .enumerate()
        {
            self.accounts.push(self.account(id, "anthropic", i));
        }
        for (i, id) in ["codex", "codex-alt"].iter().take(openai).enumerate() {
            self.accounts.push(self.account(id, "openai", i));
        }
        for a in &self.accounts {
            std::fs::create_dir_all(&a.config_dir).expect("subscription config dir");
        }
        self
    }

    fn account(&self, id: &str, provider: &'static str, i: usize) -> AccountSpec {
        let wrapper = match provider {
            "openai" => ["codex-main", "codex-alt"][i.min(1)].to_owned(),
            _ => format!("claude-{id}"),
        };
        AccountSpec {
            id: id.to_owned(),
            exec: wrapper,
            provider,
            config_dir: self.home.join("subscriptions").join(id),
            max_concurrency: 2,
            weight: 1,
        }
    }

    /// Scripts what one subscription's CLI does when it is invoked.
    pub fn scenario(mut self, account: &str, s: Scenario) -> Harness {
        let exec = self.exec_of(account);
        self.scenarios.insert(exec, s);
        self
    }

    /// Raw TOML appended to the generated config, for the cases a builder would obscure.
    pub fn with_toml(mut self, toml: &str) -> Harness {
        self.extra_toml.push_str(toml);
        self.extra_toml.push('\n');
        self
    }

    /// Makes one account win the tie every healthy account starts on, so a rotation test can
    /// say which subscription burns first.
    pub fn prefer(mut self, account: &str) -> Harness {
        for a in &mut self.accounts {
            a.weight = if a.id == account { 10 } else { 1 };
        }
        self
    }

    pub fn max_concurrency(mut self, n: usize) -> Harness {
        for a in &mut self.accounts {
            a.max_concurrency = n;
        }
        self
    }

    pub fn exec_of(&self, account: &str) -> String {
        self.accounts
            .iter()
            .find(|a| a.id == account)
            .map(|a| a.exec.clone())
            .unwrap_or_else(|| panic!("no account `{account}` in the harness"))
    }

    /// Writes the config and installs the fakes, then hands back a ready `swamp` command.
    pub fn swamp(&self, args: &[&str]) -> assert_cmd::Command {
        self.install();
        let mut cmd = assert_cmd::Command::cargo_bin("swamp").expect("the swamp binary is built");
        cmd.current_dir(&self.repo).args(args);
        for (k, v) in self.env() {
            cmd.env(k, v);
        }
        cmd.env_remove("SWAMP_DEPTH");
        cmd
    }

    /// The same invocation as a plain child process, for the tests that signal it.
    pub fn spawn(&self, args: &[&str]) -> std::process::Child {
        self.install();
        let mut cmd = std::process::Command::new(swamp_exe());
        cmd.current_dir(&self.repo)
            .args(args)
            .env_remove("SWAMP_DEPTH")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for (k, v) in self.env() {
            cmd.env(k, v);
        }
        cmd.spawn().expect("spawning swamp")
    }

    pub fn env(&self) -> Vec<(String, String)> {
        let path = std::env::var("PATH").unwrap_or_default();
        vec![
            ("PATH".to_owned(), format!("{}:{path}", self.bin)),
            ("HOME".to_owned(), self.home.to_string()),
            ("SWAMP_CONFIG_DIR".to_owned(), self.config_dir.to_string()),
            (scenario::FAKE_DIR.to_owned(), self.bin.to_string()),
        ]
    }

    pub fn install(&self) {
        std::fs::write(self.config_dir.join("config.toml"), self.config())
            .expect("writing the harness config");
        install_fakes(&self.bin, &self.all_scenarios());
    }

    /// Every configured subscription gets a wrapper; an unscripted one replays the fixture.
    fn all_scenarios(&self) -> BTreeMap<String, Scenario> {
        let mut all = BTreeMap::new();
        for a in &self.accounts {
            let default = if a.provider == "openai" {
                Scenario::codex()
            } else {
                Scenario::claude()
            };
            all.insert(
                a.exec.clone(),
                self.scenarios.get(&a.exec).cloned().unwrap_or(default),
            );
        }
        for (name, s) in &self.scenarios {
            all.insert(name.clone(), s.clone());
        }
        all
    }

    pub fn config(&self) -> String {
        let mut text = format!(
            r#"version = 1

[limits]
worker_timeout = "90s"
grace_period = "2s"
unsafe_ack = false

[dispatch]
max_attempts = 3

[journal]
redact = ['(?i)(api[_-]?key|authorization|bearer|secret|password)\s*[:=]\s*\S+']

[brain]
permission_mode = "acceptEdits"

[providers.anthropic]
models = {{ high = "{HIGH}", mid = "{MID}", low = "{LOW}" }}

[providers.openai]
models = {{ high = "{HIGH}", mid = "{MID}", low = "{LOW}" }}
"#
        );
        for a in &self.accounts {
            let key = if a.provider == "openai" {
                "CODEX_HOME"
            } else {
                "CLAUDE_CONFIG_DIR"
            };
            text.push_str(&format!(
                "\n[[accounts]]\nid = \"{}\"\nprovider = \"{}\"\nexec = \"{}\"\n\
                 max_concurrency = {}\nweight = {}\nenv = {{ {key} = \"{}\" }}\n",
                a.id, a.provider, a.exec, a.max_concurrency, a.weight, a.config_dir
            ));
        }
        text.push_str(&self.extra_toml);
        text
    }

    // ------------------------------------------------------------ inspection

    pub fn paths(&self) -> Paths {
        Paths {
            repo: self.repo.clone(),
            dot_swamp: self.repo.join(".swamp"),
            home_swamp: self.home.join(".swamp"),
        }
    }

    pub fn runs(&self) -> Vec<RunId> {
        self.paths().list_runs().expect("listing runs")
    }

    pub fn last_run(&self) -> RunPaths {
        let runs = self.runs();
        let run = *runs.first().expect("at least one run was recorded");
        self.paths().run_paths(run)
    }

    pub fn view(&self, run: RunId) -> RunView {
        RunView::load(&self.paths().run_paths(run).dir, false).expect("folding the journal")
    }

    pub fn last_view(&self) -> RunView {
        RunView::load(&self.last_run().dir, false).expect("folding the journal")
    }

    pub fn journal_text(&self, run: RunId) -> String {
        std::fs::read_to_string(self.paths().run_paths(run).journal()).expect("journal.jsonl")
    }

    /// Every recorded call of one subscription's wrapper, oldest first.
    pub fn invocations(&self, account: &str) -> Vec<Invocation> {
        let path = self
            .bin
            .join(format!("{}.invocations.jsonl", self.exec_of(account)));
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("an invocation record"))
            .collect()
    }

    pub fn brain_stdin(&self, account: &str) -> Vec<String> {
        let path = self
            .bin
            .join(format!("{}.stdin.jsonl", self.exec_of(account)));
        std::fs::read_to_string(&path)
            .map(|t| t.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// `<home>/.swamp/worktrees/<repo-name>-<hash8>`, the default layout.
    pub fn worktree_root(&self) -> Utf8PathBuf {
        self.paths().worktree_root()
    }

    pub fn accounts_state(&self) -> swamp::dispatch::persist::StateMap {
        swamp::dispatch::persist::load_state(&self.paths().accounts_state())
            .expect("reading accounts.json")
    }

    pub fn git(&self, args: &[&str]) -> String {
        git(&self.repo, args)
    }

    pub fn is_dirty(&self) -> bool {
        !self.git(&["status", "--porcelain"]).trim().is_empty()
    }
}

impl Default for Harness {
    fn default() -> Self {
        Harness::new()
    }
}

/// Puts one fake CLI on PATH per wrapper name, next to the scenario file that scripts it.
/// The name decides which fake it is, which is how a real multi-subscription setup works.
/// Returns the directory to prepend to PATH.
pub fn install_fakes(dir: &Utf8Path, scenarios: &BTreeMap<String, Scenario>) -> Utf8PathBuf {
    std::fs::create_dir_all(dir).expect("the fake bin dir");
    for (name, s) in scenarios {
        let target = if name.starts_with("codex") {
            fake_exe("fake_codex")
        } else {
            fake_exe("fake_claude")
        };
        let link = dir.join(name);
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&target, &link)
            .unwrap_or_else(|e| panic!("linking {link} -> {target}: {e}"));
        let body = serde_json::to_vec_pretty(s).expect("a scenario serializes");
        std::fs::write(dir.join(format!("{name}.json")), body).expect("writing a scenario");
    }
    dir.to_path_buf()
}

pub fn fake_exe(name: &str) -> Utf8PathBuf {
    let path = match name {
        "fake_codex" => env!("CARGO_BIN_EXE_fake_codex"),
        _ => env!("CARGO_BIN_EXE_fake_claude"),
    };
    Utf8PathBuf::from(path)
}

/// A run's control socket is a UDS, and `sun_path` is 104 bytes on macOS. The default
/// TMPDIR there is long enough on its own to blow that budget, so tests root themselves in
/// a short directory instead.
fn short_tmp() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("SWAMP_TEST_TMP") {
        return std::path::PathBuf::from(dir);
    }
    let tmp = std::path::PathBuf::from("/tmp");
    if tmp.is_dir() {
        return tmp;
    }
    std::env::temp_dir()
}

pub fn swamp_exe() -> Utf8PathBuf {
    Utf8PathBuf::from(env!("CARGO_BIN_EXE_swamp"))
}

/// Unix epoch seconds, `secs` from now: the resets_at a provider would report.
pub fn epoch_in(secs: i64) -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp() + secs
}

pub fn git(dir: &Utf8Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=swamp tests",
            "-c",
            "user.email=tests@swamp.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("running git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn init_repo(repo: &Utf8Path) {
    git(repo, &["init", "-q"]);
    std::fs::write(repo.join("README.md"), "swamp e2e repo\n").expect("README");
    std::fs::write(repo.join("api.rs"), "fn main() {}\n").expect("api.rs");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "initial commit"]);
}

/// Waits for `f`, polling, so a test never sleeps a fixed amount for a live process.
pub fn wait_for(what: &str, timeout: std::time::Duration, f: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("timed out after {timeout:?} waiting for {what}");
}
