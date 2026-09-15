//! A fake `codex` CLI: replays a scripted `codex exec --json` stream, banner line included,
//! and writes the final answer to the file named by `-o`, the way the real one does.

#[path = "scenario.rs"]
mod scenario;

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let name = scenario::wrapper_name(&argv);
    let dir = scenario::fake_dir();
    let script = scenario::load(&dir, &name);

    let stdin = scenario::read_stdin();
    let n = scenario::record(&dir, &name, &argv, &stdin);
    scenario::check_argv(&argv, &script);
    if argv.get(1).map(String::as_str) != Some("exec") {
        scenario::die(&format!("expected `exec` as the subcommand, got {argv:?}"));
    }
    scenario::apply_edits(&script, n);

    // The summary comes from this file, not from the stream: codex reports no final text.
    if let (Some(text), Some(path)) = (&script.last_message, scenario::flag(&argv, "-o")) {
        let _ = std::fs::write(path, scenario::subst(text, n));
    }

    scenario::emit(&script, n);
    for line in &script.stderr {
        eprintln!("{}", scenario::subst(line, n));
    }
    std::process::exit(script.exit_code);
}
