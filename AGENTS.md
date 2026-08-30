# Repository Guidelines

`CLAUDE.md` is a symlink to this file. Update `AGENTS.md` only.

Hive is a Rust CLI (a cargo workspace with one crate, `crates/hive/`). This
file carries the decisions and rules the code cannot state for itself. For
what a module does, read the module.

## Truth Layers

- `registry.rs` is the team truth layer — one JSON file per team under
  `$HIVE_HOME/state/teams/`. tmux is display, resolved on top of it: a pane or
  a window is never the authority on who is on a team. The write lanes are
  split — the CLI owns roster membership, the hived only backfills fields of
  names already there — so an observation racing a kill cannot resurrect the
  member that was killed.
- The `hive view` transcript viewer is a read-only mirror by construction: in
  production code the subsystem's only call out is `crate::settings` on the
  `view.theme` key. No registry, no bus, no hived. Keep it that way — the
  viewer must stay usable against a transcript file alone.
- The flow engine is Rust but its scripting surface is not: flow scripts are
  Python against the embedded client in `crates/hive/assets/pylib/`, reaching
  the engine over a hidden `flow-op` subcommand that re-enters the binary.
  Changing the flow API is therefore always a two-sided change — the Rust
  side and the embedded pylib. Half of one compiles cleanly: the compiler
  sees only the Rust side, and the pylib test shims the other.
- Embedded assets (the cvim toolkit, the flow pylib) materialize under
  `$HIVE_HOME/core_assets/` heal-on-drift at first use: any on-disk copy that
  differs from the embedded bytes is rewritten. Editing a materialized asset
  is not a way to change behavior; change the embedded copy.
- `plugins/hive/` is the Claude/Codex marketplace plugin. Its two manifests
  (`.claude-plugin/plugin.json`, `.codex-plugin/plugin.json`) carry the crate
  version; nothing enforces the match, so bumps are manual.
- The viewer's markdown engine is the pinned git dependency
  `xai-grok-markdown` (`crates/hive/Cargo.toml`), and its chrome mirrors
  grok's own pager: doc comments cite grok files by bare name (`grok
  mouse.rs`, `grok execute.rs`, `grok context_bar.rs`), whose full source sits
  in the cargo checkout at
  `~/.cargo/git/checkouts/grok-build-*/<rev>/crates/codegen/xai-grok-pager*/src/`.
  Read that source before changing a mirrored component.

## Design Docs

Where design truth lives, and which question each doc owns:

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
  protocol. Sharp edge: every claim there is pinned to one Claude Code build
  and must be re-verified on upgrade. Hive consumes only `op: "reply"`; the
  rest is recorded, not used.

## Build, Test, and Development Commands

- Live Hive agents use the stable installed `hive` binary as their
  communication transport. Do not point that live install at an in-progress
  checkout while a team is using it. Install from a committed main checkout
  (`cargo install --path crates/hive`); the live install never points at a
  dirty worktree.
- `cargo nextest run` — the whole Rust suite. nextest (one process per test)
  is required: tests mutate env vars freely, and plain `cargo test` shares one
  process and cross-contaminates.
- `python -m pytest tests/e2e -q` — black-box tmux flows against
  `target/debug/hive` (`cargo build` first, or point `HIVE_E2E_BIN` elsewhere).
- `HIVE_ACCEPTANCE=1 HIVE_ACCEPTANCE_CLIS=claude,codex,grok python -m pytest tests/acceptance -q`
  — post-install live acceptance: one real agent per CLI, spawned through the
  installed `hive`. It exists to assert what unit suites structurally cannot
  see (reply identity, absence of acks, pane color as tmux actually renders
  it, picker residue, nonce causality) plus a headless-claude semantic
  coroner. Run it after every live install; it is skipped everywhere else.
- Plugin/skill materialization and hived behavior that must exercise new
  source code need an isolated dev lane: disposable `HIVE_HOME`,
  `CLAUDE_HOME`, `CODEX_HOME`, and a temporary team/window. Do not restart the
  current live team's hived onto checkout code; the live hived stays on the
  stable install until an intentional upgrade.

## Coding Style & Naming Conventions

Rust 2021, rustfmt defaults. Match the existing style: small focused
functions, minimal comments, snake_case function names carried over from the
retired Python implementation — leading underscore included
(`_daemon_control_sock`), where it marks a Python-private ancestor and says
nothing about Rust visibility. Do not strip those prefixes.
`crates/hive/PORTING.md` records the port-era naming and JSON-compat rules.
Test names stay explicit, e.g. `test_wait_status_times_out_without_match`. Do
not leave dead code: if a function becomes a no-op or unused, delete it along
with all call sites instead of leaving an empty body.

## Testing Guidelines

Every CLI command should have test coverage at some layer; complex flows also
get e2e coverage. Add unit tests for pure logic before relying on higher-level
tests. Rust tests must not touch the real tmux server, real `~/.hive`, or the
network — integration tests in `crates/hive/tests/` that need tmux create
their own detached sessions and kill them.

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

Do not hardcode secrets, session IDs, or local machine paths.
The hived is a long-lived workspace process. When validating hived-related runtime changes manually, restart it from the current workspace before trusting `doctor`, delivery, or activity output.

## Debug Logs

`hive doctor` prints the current workspace `runDir` and its `logs` map; read
the paths from there rather than hardcoding one. Two things those paths do not
tell you:

- `run/cvim/` is written by the embedded cvim bash toolkit
  (`assets/cvim/bin/cvim-command`), not by Rust, with `latest` naming the
  newest run. Grepping the crate's Rust source for the writer finds nothing.
- Log verbosity defaults to `normal`, which drops the three highest-frequency
  hived events (`DEV_ONLY_EVENTS` in `devlog.rs`); every other notify event is
  recorded either way, and the gate is notify-only. An event missing from
  `notify.jsonl` is not evidence that it never fired. Use
  `HIVE_LOG_VERBOSITY=dev` only as a temporary debugging escape hatch.

When no workspace resolves, both `notify.jsonl` and `cvim/` fall back under
`${XDG_CACHE_HOME:-~/.cache}/hive/` — that is also where a cvim run from an
untagged pane lands.
