# Repository Guidelines

`CLAUDE.md` is a symlink to this file. Update `AGENTS.md` only.

## Project Structure & Module Organization

Hive is a small Python CLI project. Main code lives in `src/hive/`:
- `cli.py` defines the Click command surface.
- `agent.py`, `team.py`, and `tmux.py` implement runtime behavior.
- `bus.py` and `context.py` handle workspace state and per-pane context.
- `flow.py` is the deterministic orchestration library behind `hive flow run`.

Tests live under `tests/` and are split by level:
- `tests/unit/` for isolated logic
- `tests/cli/` for command behavior with mocks
- `tests/e2e/` for real tmux-backed flows

## Design Docs

- Runtime design lives in `docs/runtime-model.md`.
- Keep runtime-field semantics there in sync with code:
  - `busy`
  - `inputState`
  - `turnPhase`
- `CLAUDE.md` is only a symlink entrypoint to this file. Do not edit it separately.

## Build, Test, and Development Commands

- Live Hive agents use the stable global/network-installed `hive` binary as their communication transport. Do not point that live install at an in-progress checkout while a team is using it.
- Development and tests run against source explicitly:
  ```
  PYTHONPATH=src python -m pytest tests/ -q
  ```
- `PYTHONPATH=src python -m pytest tests/ -m unit -q` — fast unit tests only.
- `PYTHONPATH=src python -m pytest tests/ -m cli -q` — CLI-layer tests.
- `PYTHONPATH=src python -m pytest tests/ -m e2e -q` — end-to-end tmux tests.
- `PYTHONPATH=src python -m pytest tests/unit/test_cvim_command.py tests/unit/test_cvim_payload.py -q` — focused `/cvim` and `/vim` sendback coverage.
- Plugin/skill materialization and sidecar behavior that must exercise new source code need an isolated dev lane: disposable `HIVE_HOME`, `CLAUDE_HOME`, `CODEX_HOME`, and a temporary team/window. Do not restart the current live team's sidecar onto checkout code; the live sidecar stays on the stable install until an intentional upgrade.

## Coding Style & Naming Conventions

Use Python 3.11+ with 4-space indentation and type hints where practical. Match the existing style: small focused functions, minimal comments, and straightforward dataclass-based models. File names are lowercase with underscores. Test names should be explicit, e.g. `test_wait_status_times_out_without_match`. Do not leave dead code: if a function becomes a no-op or unused, delete it along with all call sites instead of leaving an empty body.

## Testing Guidelines

Every CLI command should have at least one CLI test and complex flows should also have e2e coverage. Add unit tests for pure logic before relying on higher-level tests. Keep new tests in the correct layer and use shared fixtures from `tests/conftest.py` or helpers in `tests/e2e/_helpers.py`.

Do not test hand-written prose by locking exact words. Forbidden: tests that read repo-authored docs, specs, prompts, or skill text (`AGENTS.md`, `README.md`, `plugins/**/skills/**/SKILL.md`) and assert that specific phrases or headings are present or absent. Review prose changes by reading the diff.

Allowed: tests that read generated files, state files, JSON, scripts, or payloads to verify executable behavior. Prefer assertions on command exit codes, structured fields, files created, parser output, tmux side effects, and other runtime contracts. If prose must control behavior, move the contract into code or structured data and test that boundary instead of literal wording.

When touching `/cvim` popup sendback behavior, keep `tests/unit/test_cvim_command.py::test_popup_schedules_post_after_popup_exits` passing. It guards the regression where `run-shell` was started before popup teardown completed, causing the returned edit payload to be swallowed.

## Commit & Pull Request Guidelines

Follow the existing history style: short conventional messages such as `fix: ...`, `refactor: ...`, or `docs: ...`. Keep commits scoped to one logical change. Before opening a PR, run the relevant pytest targets, summarize the behavioral change, and call out tmux assumptions or manual verification steps.

## Version Bump

Only bump when the user explicitly says `bump`（或 `commit push bump`）. Normal `commit push` does **not** bump.

When bumping, scan all commits since the last version bump commit and determine the level automatically:

1. Find the last commit that touched `pyproject.toml` version (or the last `chore: bump version` commit).
2. Collect all commit headers between that point and HEAD.
3. Determine bump level from the **highest impact** in that range:
   - Bump **minor** only when there is a large user-facing feat: a genuinely new capability, workflow, or command surface, or a significant change in default behavior or external integration (e.g. 0.4.0 → 0.5.0)
   - Everything else is **patch**, including internal `feat:` improvements, reliability/performance, diagnostics, help/docs/skill text, refactors, and polish or surfacing of existing behavior (e.g. 0.4.0 → 0.4.1)
   - **Judgement test**: 问"user / agent 能做的真·新事情是什么?"。如果答案是"以前就能做,只是换了名字 / 修好了会崩的场景",就是 patch
   - **Patch traps**(这些看起来像 minor,实则是 patch):修 bug 顺带加的 override / escape-hatch flag、重命名 scheme、tag key 翻新、新 debug 子命令。即使单 commit 带 `feat:` 前缀也不自动提级
   - When in doubt, default to **patch**
4. **Never auto-bump major.** If any commit has breaking changes (`!` suffix or `BREAKING CHANGE`), ask the user.
5. Edit `pyproject.toml` version, commit as `chore: bump version to X.Y.Z`, then push.

## Security & Runtime Notes

Do not hardcode secrets, session IDs, or local machine paths. Hive depends on `tmux`; e2e tests assume tmux is available and cover the CLI-only surfaces (agent spawn/delivery flows are cli-layer tests with mocks).
The sidecar is a long-lived workspace process. When validating sidecar-related runtime changes manually, restart it from the current workspace before trusting `doctor`, delivery, or activity output.

## Debug Log Locations

`hive doctor` includes the current workspace `runDir` and `logs` map. Prefer those paths when debugging a specific team:
- `<workspace>/run/notify.jsonl` — notify UI and idle watcher state-machine events.
- `<workspace>/run/sidecar.stderr` — sidecar stderr and uncaught process-level failures.
- `<workspace>/run/cvim/` — per-run JSONL logs for `hive cvim` / `hive vim`; `latest` points to the newest run.

When no workspace can be resolved, logs fall back under `${XDG_CACHE_HOME:-~/.cache}/hive/`:
- `notify.jsonl`
- `cvim/`

Log verbosity defaults from install mode: source checkout/editable installs use `dev`, while `site-packages` / `dist-packages` installs use `normal`. `normal` only filters low-information sidecar heartbeat events; business-path notify and cvim events are still recorded. Use `HIVE_LOG_VERBOSITY=dev|normal` only as a temporary debugging escape hatch.
