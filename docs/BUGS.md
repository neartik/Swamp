# Bugs

## CTRL + D

Status: resolved.

When using CTRL + D, it hangs before quitting.

Ctrl+D on an empty editor quit without interrupting the turn, and brain shutdown then waited up
to `limits.brain_turn_timeout` (15 minutes) for the CLI to wind down. Ctrl+D now behaves like an
armed Ctrl+C (interrupt, cancel all, quit 0 or 6 mid-turn), does nothing when the editor holds
text, and shutdown waits at most `limits.grace_period` before killing the brain.

Tests:
- `src/ui/chat/screens.rs`: `ctrl_d_on_an_empty_editor_quits_like_an_armed_ctrl_c`,
  `ctrl_d_with_text_in_the_editor_does_nothing`
- `tests/brain_session.rs`: `a_claude_brain_that_ignores_eof_is_cut_off_after_the_grace_period`,
  `a_codex_turn_that_never_ends_is_cut_off_after_the_grace_period`

## Brain activity

Status: resolved.

The brain doesn't delegate properly, it tries to do all while it should delegate asap to lower
models / effort.

The system prompt now sets a read budget before the first `swamp_dispatch`
(`limits.brain_read_budget`, default 8), sends investigation out as a low tier task, and starts
the tier rubric at low: low for mechanical and exploratory work, mid for normal changes, high
only for design and review.

Tests:
- `tests/brain_session.rs`: `the_prompt_sets_a_read_budget_and_starts_at_low_tier`, with the
  `system_prompt_interactive` and `system_prompt_one_shot` snapshots

## Tools

Status: resolved.

Tools are failing due to permission. It should run --yolo or auto for Claude.

Not fixed with `bypassPermissions` or `auto` (which denies file writes): the default Anthropic
worker now gets the brain's pairing, `acceptEdits` plus an explicit allowed-tools list (Bash,
Read, Grep, Glob, Edit, Write, MultiEdit). `swamp chat` and `swamp run` run the doctor
permission check at startup and print a warning, never a refusal, when a configured mode would
deny Bash.

Tests:
- `tests/config_load.rs`: `the_default_anthropic_worker_may_run_bash_under_accept_edits`
- `tests/parse_claude.rs`:
  `the_default_worker_accepts_edits_and_may_run_bash_without_bypassing_permissions`,
  `read_only_isolation_still_denies_the_edit_tools_the_default_allows`
- `tests/doctor.rs`: `the_default_config_gives_no_permission_warning`
