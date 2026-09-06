# Repository guidelines

`CLAUDE.md` is a symlink to this file. Update `AGENTS.md` only.

Hive is a Rust CLI (a cargo workspace with one crate, `crates/hive/`). This
file records the decisions and rules the code cannot state itself. Module
behavior is documented in the modules themselves.

## Truth layers

- `registry.rs` is the team truth layer: one directory per team,
  `$HIVE_HOME/teams/<team>/`, whose `team.json` is the registry entry — a
  directory without one is not a team. That directory is also the team's
  default workspace (`hive.db`, `run/`, `artifacts/`); an explicit
  `--workspace` lives elsewhere and the entry's `workspace` field records
  it. `hive create` on the default resets it (a recycled pool name must not
  inherit its predecessor's bus); `hive delete` removes `team.json` only,
  `--delete-workspace` the whole directory. The store lock is
  `$HIVE_HOME/teams/.lock`. tmux is display, resolved on top of it, and a
  pane or a window is not the authority on who is on a team. The orch
  mirror is display state of the same kind: `@hive-role mirror` on the
  pane, `@hive-mirror on|off` on the team window (`hive mirror`; unset
  means open), `@hive-hidden <team>` on the window that parks a closed
  mirror pane, and nothing in the registry. The team session's status bar
  (`tmux/status.rs`) is rendered from tmux options alone —
  `@hive-busy`/`@hive-unread` per pane and `@hive-ticker` per window are
  the hived's display writes (`hived/status.rs`), never authority. The
  write lanes are split, with the CLI owning roster membership and the
  hived only backfilling fields of names already there, so an observation
  racing a kill cannot resurrect the member that was killed.
- The `hive view` transcript viewer is a read-only mirror by construction: in
  production code the subsystem's only call out is `crate::settings` on the
  `view.theme` key, with no registry, bus, or hived access. Keep it that way;
  the viewer must stay usable against a transcript file alone.
- The flow engine is Rust; its scripting surface is JavaScript, evaluated
  in-process by the embedded QuickJS runtime (`flow_script.rs`). The whole
  dialect lives in that module's JS prelude; the only host primitive is one
  op seam into `flow.rs::run_op`, and both sides are covered by the same
  test suite — there is no external client to keep in sync. Flow scripts
  are deterministic by contract (`Date.now()`/`Math.random()`/argless
  `new Date()` throw) so a run's op journal can replay on
  `hive flow run --resume`; keep op args free of run-relative values (paths
  with counters, timestamps), because the typed op's serialization
  (`flow.rs::FlowOp`) is the journal key. `hive flow node run` is the same
  op core as one blocking command; the plugin's `hive-node` agent is only a
  relay for it, and `hive flow board` reads phases off the members' pane
  groups (`@hive-group`), never a sidecar file.
- Embedded assets (the cvim toolkit) materialize under
  `$HIVE_HOME/core_assets/` heal-on-drift at first use: any on-disk copy that
  differs from the embedded bytes is rewritten. Editing a materialized asset
  is not a way to change behavior; change the embedded copy.
- `plugins/hive/` is the Claude/Codex plugin payload, embedded into the
  binary and served from a local marketplace materialized under
  `$HIVE_HOME/core_assets/marketplace/` by `hive plugin sync`. Claude
  consumes it as a command source (the command re-runs once per session, so
  skills track the binary); codex as a directory source whose cache is keyed
  by the manifest version. The two manifests (`.claude-plugin/plugin.json`,
  `.codex-plugin/plugin.json`) carry the crate version. Bumps are manual;
  `cargo nextest run` fails when the claude manifest drifts from
  `CARGO_PKG_VERSION` (`plugin_manager.rs`), the codex manifest is not
  checked, and a missed codex bump only costs an idempotent re-add at the
  next codex launch. The plugin ships no hooks at all: codex gates plugin
  hooks behind a hook-review dialog that would block unattended members, so
  the codex re-add lives in hive's launch path
  (`ensure_codex_plugin_current`), and the claude side needs none — the
  command source is the sync.
- The viewer's markdown engine is the pinned git dependency
  `xai-grok-markdown` (`crates/hive/Cargo.toml`), and its chrome mirrors
  grok's own pager: doc comments cite grok files by bare name (`grok
  mouse.rs`, `grok execute.rs`, `grok context_bar.rs`), whose full source sits
  in the cargo checkout at
  `~/.cargo/git/checkouts/grok-build-*/<rev>/crates/codegen/xai-grok-pager*/src/`.
  Read that source before changing a mirrored component.

## Design docs

Design truth lives in these docs, one question each:

- `docs/runtime-model.md` — what hive knows about an engine: team identity
  (registry vs tmux display), the runtime fields and their per-CLI native
  sources, send addressing, active-turn fork routing. Runtime-field semantics
  belong there; keep them in sync with the code that computes them.
- `docs/transcript-view.md` — what a reader sees on screen: the JSONL →
  `DisplayBlock` parse model, the TUI's chrome and interaction layer, theme
  and appearance resolution. The boundary against `runtime-model.md` follows
  the code: the viewer holds none of the runtime state `runtime-model.md`
  defines and feeds none back.
- `docs/daemon-control-socket.md` — the Claude bg supervisor daemon's control
  protocol. Every claim there is pinned to one Claude Code build and must be
  re-verified on upgrade. Hive consumes only `op: "reply"`; the rest is
  recorded, not used.
- `docs/notify-effects.md` — the notify attention effect
  (`notify_ui.rs`): what a fire marks on the window and pane, what the
  select-window hook clears, and why hive draws nothing itself.

## Build, test, and development commands

- Live Hive agents use the stable installed `hive` binary as their
  communication transport. Do not point that live install at an in-progress
  checkout while a team is using it. Install from a committed main checkout
  (`cargo install --path crates/hive`); the live install never points at a
  dirty worktree.
- `cargo nextest run` — the whole Rust suite. nextest (one process per test)
  is required: the hived test hooks (`hived/testhook.rs`) are process-global
  state, not per-test, and plain `cargo test` races them in one process.
- `python -m pytest tests/e2e -q` — black-box tmux flows against
  `target/debug/hive` (`cargo build` first, or point `HIVE_E2E_BIN` elsewhere).
- `HIVE_ACCEPTANCE=1 HIVE_ACCEPTANCE_CLIS=claude,codex,grok python -m pytest tests/acceptance -q`
  — post-install live acceptance: one real agent per CLI, spawned through the
  installed `hive`. It asserts what unit suites structurally cannot see (reply
  identity, absence of acks, pane color as tmux actually renders it, picker
  residue, nonce causality) plus a headless-claude semantic coroner. Run it
  after every live install; it is skipped everywhere else.
- Plugin/skill materialization and hived behavior that must exercise new
  source code need an isolated dev lane: disposable `HIVE_HOME`,
  `CLAUDE_HOME`, `CODEX_HOME`, and a temporary team/window. Do not restart the
  current live team's hived onto checkout code; the live hived stays on the
  stable install until an intentional upgrade.

## Coding style & naming conventions

Rust 2021, rustfmt defaults. Match the existing style: small focused
functions, minimal comments, snake_case function names carried over from the
retired Python implementation, leading underscore included
(`_daemon_control_sock`), where it marks a Python-private ancestor and says
nothing about Rust visibility. Do not strip those prefixes.
`crates/hive/PORTING.md` records the port-era naming and JSON-compat rules.
Test names stay explicit, e.g. `test_wait_status_times_out_without_match`. Do
not leave dead code: if a function becomes a no-op or unused, delete it along
with all call sites instead of leaving an empty body.

## Testing guidelines

Every CLI command should have test coverage at some layer; complex flows also
get e2e coverage. Add unit tests for pure logic before relying on higher-level
tests. Rust tests must not touch the real tmux server, real `~/.hive`, or the
network. A unit test that rewrites process env holds `testenv::EnvGuard` (the
one crate-wide env lock, restoring every variable it touched on drop), so env
state never leaks between tests; nextest stays required for the reason above.
Integration tests in `crates/hive/tests/` that need tmux create their own
detached sessions and kill them. Those tests treat tmux as a hard
requirement: with no tmux binary reachable they panic (via
`common::require_tmux`) rather than pass silently, so a missing tmux is a
loud failure, not green. The Python e2e suite (`tests/e2e/`) is the same
kind of black-box coverage against the built binary, but skips when tmux is
absent — CI without tmux still collects it.

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
passing.

## Commit & pull request guidelines

Follow the existing history style: short conventional messages such as
`fix: ...`, `refactor: ...`, or `docs: ...`. Keep commits scoped to one
logical change. Before opening a PR, run `cargo nextest run` and the e2e
suite, summarize the behavioral change, and call out tmux assumptions or
manual verification steps.

## Version bump

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

## Security & runtime notes

Do not hardcode secrets, session IDs, or local machine paths.
The hived is a long-lived workspace process. When validating hived-related runtime changes manually, restart it from the current workspace before trusting `doctor`, delivery, or activity output.

## Debug logs

`hive doctor` prints the current workspace `runDir` and its `logs` map; read
the paths from there rather than hardcoding one. Two facts those paths do not
carry:

- `run/cvim/` is written by the embedded cvim bash toolkit
  (`assets/cvim/bin/cvim-command`), not by Rust, with `latest` naming the
  newest run. Grepping the crate's Rust source for the writer finds nothing.
- Log verbosity defaults to `normal`, which drops the three highest-frequency
  hived events (`DEV_ONLY_EVENTS` in `devlog.rs`); every other notify event is
  recorded either way, and the gate is notify-only. An event missing from
  `notify.jsonl` is not evidence that it never fired. Use
  `HIVE_LOG_VERBOSITY=dev` only as a temporary debugging escape hatch.

When no workspace resolves, both `notify.jsonl` and `cvim/` fall back under
`${XDG_CACHE_HOME:-~/.cache}/hive/`, which is also where a cvim run from an
untagged pane lands.
