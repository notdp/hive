# Repository Guidelines

`CLAUDE.md` is a symlink to this file. Update `AGENTS.md` only.

## Project Structure & Module Organization

Hive is a Rust CLI (a cargo workspace with one crate). Main code lives in
`crates/hive/src/`:
- `cli/` defines the clap command surface (`mod.rs` tree + helpers,
  `core_cmds.rs` registry-truth verbs, `rest.rs` everything else,
  `help_text.rs` byte-locked help output).
- `agent.rs`, `team.rs`, and `tmux.rs` implement runtime behavior.
- `bus.rs` and `context.rs` handle workspace state and per-pane context.
- `flow.rs` is the orchestration engine behind `hive flow run`; flow scripts
  stay Python (`from hive.flow import agent, parallel`) against the embedded
  client in `crates/hive/assets/pylib/`, bridged over the hidden `flow-op`
  command.
- `hived.rs` is the per-team daemon (`hive --hived …` re-enters the binary).
- `adapters/` hold the per-CLI transports (claude/codex/grok).
- `crates/hive/assets/` are embedded data files (cvim toolkit, pylib,
  plugins) materialized to `$HIVE_HOME/core_assets/` at first use.
- `crates/hive/PORTING.md` records the Python→Rust port conventions.

Tests:
- Rust unit tests live in `#[cfg(test)]` blocks next to the code; real-tmux
  integration tests in `crates/hive/tests/` (own detached sessions only).
- `tests/e2e/` — Python black-box tests driving the built binary in tmux.
- `tests/acceptance/` — post-install live acceptance (real agents).

## Design Docs

- Runtime design lives in `docs/runtime-model.md`.
- Keep runtime-field semantics there in sync with code:
  - `busy`
  - `inputState`
  - `turnPhase`
- `CLAUDE.md` is only a symlink entrypoint to this file. Do not edit it separately.

## Build, Test, and Development Commands

- Live Hive agents use the stable installed `hive` binary as their
  communication transport. Do not point that live install at an in-progress
  checkout while a team is using it.
- `cargo build` — debug binary at `target/debug/hive`.
- `cargo nextest run` — the whole Rust suite. nextest (one process per test)
  is required: tests mutate env vars freely and plain `cargo test` shares one
  process and cross-contaminates.
- `python -m pytest tests/e2e -q` — black-box tmux flows against
  `target/debug/hive` (or `HIVE_E2E_BIN=<path>` to point elsewhere).
- `HIVE_ACCEPTANCE=1 HIVE_ACCEPTANCE_CLIS=claude,codex,grok python -m pytest tests/acceptance -q`
  — post-install live acceptance: spawns one real member per CLI through the
  installed `hive` and asserts the oracles unit suites cannot see (reply
  identity, no acks, pane color via `capture-pane -e`, no picker residue,
  nonce causality) plus a headless-claude semantic coroner. Run it after
  every live install; it is skipped everywhere else.
- Install: `cargo install --path crates/hive` (from a committed main
  checkout); the live install never points at a dirty worktree.
- Plugin/skill materialization and hived behavior that must exercise new
  source code need an isolated dev lane: disposable `HIVE_HOME`,
  `CLAUDE_HOME`, `CODEX_HOME`, and a temporary team/window. Do not restart
  the current live team's hived onto checkout code; the live hived stays on
  the stable install until an intentional upgrade.

## Coding Style & Naming Conventions

Rust 2021, rustfmt defaults. Match the existing style: small focused
functions, minimal comments, snake_case function names carried over from the
Python spec (see `crates/hive/PORTING.md`). Test names stay explicit, e.g.
`test_wait_status_times_out_without_match`. Do not leave dead code: if a
function becomes a no-op or unused, delete it along with all call sites
instead of leaving an empty body.

## Testing Guidelines

Every CLI command should have test coverage at some layer; complex flows
also get e2e coverage. Add unit tests for pure logic before relying on
higher-level tests. Rust tests must not touch the real tmux server, real
`~/.hive`, or the network — integration tests in `crates/hive/tests/` that
need tmux create their own detached sessions and kill them.

Do not test hand-written prose by locking exact words. Forbidden: tests that
read repo-authored docs, specs, prompts, or skill text (`AGENTS.md`,
`README.md`, `plugins/**/skills/**/SKILL.md`) and assert that specific
phrases or headings are present or absent. Review prose changes by reading
the diff.

Allowed: tests that read generated files, state files, JSON, scripts, or
payloads to verify executable behavior. Prefer assertions on command exit
codes, structured fields, files created, parser output, tmux side effects,
and other runtime contracts. If prose must control behavior, move the
contract into code or structured data and test that boundary instead of
literal wording.

When touching `/cvim` popup sendback behavior, keep
`crates/hive/tests/cvim_command.rs::test_popup_schedules_post_after_popup_exits`
passing. It guards the regression where `run-shell` was started before popup
teardown completed, causing the returned edit payload to be swallowed.

## Commit & Pull Request Guidelines

Follow the existing history style: short conventional messages such as
`fix: ...`, `refactor: ...`, or `docs: ...`. Keep commits scoped to one
logical change. Before opening a PR, run `cargo nextest run` and the e2e
suite, summarize the behavioral change, and call out tmux assumptions or
manual verification steps.

## Version Bump

Only bump when the user explicitly says `bump`（或 `commit push bump`）. Normal `commit push` does **not** bump.

When bumping, scan all commits since the last version bump commit and determine the level automatically:

1. Find the last commit that touched the version in `crates/hive/Cargo.toml` (or the last `chore: bump version` commit).
2. Collect all commit headers between that point and HEAD.
3. Determine bump level from the **highest impact** in that range:
   - Bump **minor** only when there is a large user-facing feat: a genuinely new capability, workflow, or command surface, or a significant change in default behavior or external integration (e.g. 0.4.0 → 0.5.0)
   - Everything else is **patch**, including internal `feat:` improvements, reliability/performance, diagnostics, help/docs/skill text, refactors, and polish or surfacing of existing behavior (e.g. 0.4.0 → 0.4.1)
   - **Judgement test**: 问"user / agent 能做的真·新事情是什么?"。如果答案是"以前就能做,只是换了名字 / 修好了会崩的场景",就是 patch
   - **Patch traps**(这些看起来像 minor,实则是 patch):修 bug 顺带加的 override / escape-hatch flag、重命名 scheme、tag key 翻新、新 debug 子命令。即使单 commit 带 `feat:` 前缀也不自动提级
   - When in doubt, default to **patch**
4. **Never auto-bump major.** If any commit has breaking changes (`!` suffix or `BREAKING CHANGE`), ask the user.
5. Edit the version in `crates/hive/Cargo.toml` (plugin manifests are
   version-locked to it), commit as `chore: bump version to X.Y.Z`, then push.

## Security & Runtime Notes

Do not hardcode secrets, session IDs, or local machine paths. Hive depends on `tmux`; e2e tests assume tmux is available.
The hived is a long-lived workspace process. When validating hived-related runtime changes manually, restart it from the current workspace before trusting `doctor`, delivery, or activity output.

## Debug Log Locations

`hive doctor` includes the current workspace `runDir` and `logs` map. Prefer those paths when debugging a specific team:
- `<workspace>/run/notify.jsonl` — notify UI and idle watcher state-machine events.
- `<workspace>/run/hived.stderr` — hived stderr and uncaught process-level failures.
- `<workspace>/run/cvim/` — per-run JSONL logs for `hive cvim` / `hive vim`; `latest` points to the newest run.

When no workspace can be resolved, logs fall back under `${XDG_CACHE_HOME:-~/.cache}/hive/`:
- `notify.jsonl`
- `cvim/`

Log verbosity defaults to `normal`, which only filters low-information hived
heartbeat events; business-path notify and cvim events are still recorded.
Use `HIVE_LOG_VERBOSITY=dev|normal` only as a temporary debugging escape hatch.
