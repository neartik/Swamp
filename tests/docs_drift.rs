//! The docs name the same journal event tags and subcommands the code has.

use camino::Utf8PathBuf;
use clap::CommandFactory;
use std::collections::BTreeSet;

fn doc(name: &str) -> String {
    let path = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
}

/// Every `ev` tag `JournalEvent` accepts, in declaration order, as serde itself lists them.
fn code_events() -> Vec<String> {
    let err = serde_json::from_str::<swamp::JournalEvent>(r#"{"ev":"__none__"}"#)
        .expect_err("no such event");
    let text = err.to_string();
    let list = text
        .split("expected one of ")
        .nth(1)
        .unwrap_or_else(|| panic!("serde no longer lists the variants: {text}"));
    list.split(',')
        .filter_map(|v| v.split('`').nth(1))
        .map(str::to_owned)
        .collect()
}

/// The section of `text` under `heading`, up to the next heading of the same level.
fn section<'a>(text: &'a str, heading: &str) -> &'a str {
    let level = heading.split(' ').next().unwrap_or("##");
    let start = text
        .find(&format!("\n{heading}\n"))
        .unwrap_or_else(|| panic!("no `{heading}` section"));
    let body = &text[start + heading.len() + 2..];
    let end = body.find(&format!("\n{level} ")).unwrap_or(body.len());
    &body[..end]
}

fn snake(camel: &str) -> String {
    let mut out = String::new();
    for (i, c) in camel.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn compare(what: &str, code: &[String], docs: &[String]) {
    let code_set: BTreeSet<&String> = code.iter().collect();
    let doc_set: BTreeSet<&String> = docs.iter().collect();
    let missing: Vec<&&String> = code_set.difference(&doc_set).collect();
    let stale: Vec<&&String> = doc_set.difference(&code_set).collect();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "{what} drifted from JournalEvent: missing {missing:?}, no longer in the code {stale:?}"
    );
    assert_eq!(docs.len(), doc_set.len(), "{what} lists an event twice");
}

#[test]
fn the_code_lists_every_journal_event() {
    let events = code_events();
    assert!(events.len() >= 28, "{events:?}");
    assert_eq!(events.first().map(String::as_str), Some("run_started"));
}

#[test]
fn dispatch_md_lists_every_journal_event() {
    let text = doc("docs/DISPATCH.md");
    let table = section(&text, "## Journal events");
    let listed: Vec<String> = table
        .lines()
        .skip_while(|l| !l.starts_with("|---"))
        .filter(|l| l.starts_with("| `"))
        .filter_map(|l| l.split('`').nth(1))
        .map(str::to_owned)
        .collect();
    compare(
        "docs/DISPATCH.md \"Journal events\"",
        &code_events(),
        &listed,
    );
}

#[test]
fn design_md_schema_lists_every_journal_event() {
    let text = doc("docs/DESIGN.md");
    let schema = section(&text, "### 7.2 Schema");
    let body = schema
        .split("pub enum JournalEvent {")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("the JournalEvent block in DESIGN §7.2");
    let listed: Vec<String> = body
        .lines()
        .filter_map(|l| l.strip_prefix("    "))
        .filter(|l| l.starts_with(|c: char| c.is_ascii_uppercase()))
        .filter_map(|l| l.split([' ', '{']).next())
        .map(snake)
        .collect();
    compare("DESIGN §7.2", &code_events(), &listed);
}

/// Every command `swamp --help` shows has a row in the README's command table.
#[test]
fn the_readme_command_table_names_every_command() {
    let text = doc("README.md");
    let table = section(&text, "## Commands");
    let cli = swamp::Cli::command();
    let missing: Vec<&str> = cli
        .get_subcommands()
        .filter(|c| !c.is_hide_set())
        .map(|c| c.get_name())
        .filter(|name| !table.contains(&format!("`swamp {name}")))
        .collect();
    assert!(
        missing.is_empty(),
        "README ## Commands has no row for {missing:?}"
    );
}
