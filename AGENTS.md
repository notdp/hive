# Repository Guidelines

`CLAUDE.md` is a symlink to this file. Update `AGENTS.md` only.

## Project Structure & Module Organization

Hive is a Rust CLI (a cargo workspace with one crate). Main code lives in
`crates/hive/src/`:
- `cli/` defines the clap command surface (`mod.rs` tree + helpers,
  `core_cmds.rs` registry-truth verbs, `rest.rs` everything else,
  `help_text.rs` byte-locked help output).
- `agent.rs`, `team.rs`, and `tmux.rs` implement runtime behavior.
- `registry.rs` is the team truth layer — one JSON file per team under
  `$HIVE_HOME/state/teams/`, with the write lanes split (CLI owns roster
  membership, the hived only backfills). tmux is display resolved on top.
- `bus.rs` and `context.rs` handle workspace state (sqlite `hive.db` plus
  `artifacts/ state/ run/`) and per-pane context.
- `flow.rs` is the orchestration engine behind `hive flow run`; flow scripts
  stay Python (`from hive.flow import agent, parallel`) against the embedded
  client in `crates/hive/assets/pylib/`, bridged over the hidden `flow-op`
  command.
- `hived.rs` is the per-team daemon (`hive --hived …` re-enters the binary).
- `adapters/` hold the per-CLI transports (claude/codex/grok).
- The `hive view` transcript viewer is four modules: `transcript_view.rs`
  folds a Claude session JSONL into typed `DisplayBlock`s and picks the
  renderer by `isatty(1)` — TUI on a tty, legacy plain-ANSI stream into a
  pipe (`transcript_view.rs:1614`); `transcript_tui.rs` is the ratatui
  renderer (`run()` at `transcript_tui.rs:2465`);
  `transcript_tui/interact.rs` is its pure interaction state (selection,
  fold/density, the `/theme /view /find /quit` palette); `view_theme.rs`
  resolves the grokday/groknight palette. They are a read-only mirror by
  construction: `crate::settings` (key `view.theme`) is their only call out
  of the subsystem — no registry, bus, or hived. See
  `docs/transcript-view.md`.
- The viewer's markdown engine is the pinned git dependency
  `xai-grok-markdown` (`crates/hive/Cargo.toml`, rev `bc7f02e`), and its
  chrome mirrors grok's own pager: doc comments cite grok files by bare name
  (`grok mouse.rs`, `grok execute.rs`, `grok context_bar.rs`), whose full
  source sits in the cargo checkout at
  `~/.cargo/git/checkouts/grok-build-*/<rev>/crates/codegen/xai-grok-pager*/src/`.
  Read that source before changing a mirrored component.
- `crates/hive/assets/` is compile-time embedded data with three fates: the
  cvim toolkit and the flow pylib are materialized heal-on-drift under
  `$HIVE_HOME/core_assets/` at first use (`cvim.rs:26`, `flow.rs:590`); the
  `notify` plugin is written to `$HIVE_HOME/plugins/installed/` on enable
  (`plugin_manager.rs:44`); the two grok `.tmTheme` palettes never reach
  disk — `include_bytes!` at `transcript_view.rs:137`.
- `plugins/hive/` is the Claude/Codex marketplace plugin (skills, hooks,
  scripts) published through `.claude-plugin/marketplace.json`. Its two
  manifests (`.claude-plugin/plugin.json`, `.codex-plugin/plugin.json`)
  carry the crate version; nothing enforces the match, so bumps are manual.
- `crates/hive/PORTING.md` records the Python→Rust port conventions.

Tests:
- Rust unit tests live in `#[cfg(test)]` blocks next to the code; real-tmux
  integration tests in `crates/hive/tests/` (own detached sessions only).
- `tests/e2e/` — Python black-box tests driving the built binary in tmux.
- `tests/acceptance/` — post-install live acceptance (real agents).

## Design Docs

- Runtime design lives in `docs/runtime-model.md`: team identity (registry vs
  tmux display), the runtime fields and their per-CLI native sources, send
  addressing, active-turn fork routing.
- Keep runtime-field semantics there in sync with code:
  - `busy`
  - `inputState`
  - `turnPhase`
- `docs/transcript-view.md` owns `hive view`: the JSONL → `DisplayBlock`
  parse model, the TUI's chrome and interaction layer, theme and appearance
  resolution. The boundary against `runtime-model.md` follows the code —
  the viewer reads a transcript file and `settings`, nothing else, so it
  holds none of the runtime state `runtime-model.md` defines and feeds none
  back. What hive knows about an engine → `runtime-model.md`; what a reader
  sees on screen → `transcript-view.md`.
- `docs/daemon-control-socket.md` records the Claude bg supervisor daemon's
  control protocol. Sharp edge: every claim is pinned to Claude Code
  2.1.240 and must be re-verified on upgrade. Hive consumes only
  `op: "reply"` (`adapters/claude_sessions.rs:478`); the rest is recorded,
  not used.
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
retired Python implementation — leading underscore included
(`pub fn _daemon_control_sock`, `adapters/claude_sessions.rs:417`), where it
marks a Python-private ancestor and says nothing about Rust visibility. Do
not strip those prefixes. `crates/hive/PORTING.md` is the port-era record of
these conventions; it still describes a `src/hive/` spec tree and a
`tests/unit/` suite, neither of which exists any more, so read it for naming
and JSON-compat rules, not for repo layout. Test names stay explicit, e.g.
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
- `<workspace>/run/cvim/` — advertised by the `logs` map (`devlog.rs:63`) but
  empty: nothing writes it. `hive cvim` / `hive vim` emit no per-run JSONL, so
  debug those against tmux state and the popup's sendback payload instead.

When no workspace can be resolved, `notify.jsonl` falls back to
`${XDG_CACHE_HOME:-~/.cache}/hive/notify.jsonl` (`devlog.rs:19`).

Log verbosity defaults to `normal`, which drops exactly three high-frequency
hived events — `active.changed`, `tick.summary`, `windows.changed`
(`devlog.rs:15`); every other notify event is recorded either way. The gate is
notify-only. Use `HIVE_LOG_VERBOSITY=dev|normal` only as a temporary debugging
escape hatch.
