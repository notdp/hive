# Repository Guidelines

`CLAUDE.md` is a symlink to this file. Update `AGENTS.md` only.

## Project Structure & Module Organization

Hive is a small Python CLI project. Main code lives in `src/hive/`:
- `cli.py` defines the Click command surface.
- `agent.py`, `team.py`, and `tmux.py` implement runtime behavior.
- `bus.py` and `context.py` handle workspace state and per-pane context.

Tests live under `tests/` and are split by level:
- `tests/unit/` for isolated logic
- `tests/cli/` for command behavior with mocks
- `tests/e2e/` for real tmux-backed flows

## Build, Test, and Development Commands

- `python3 -m pip install -e .` — install Hive in editable mode.
- **MUST**: after ANY code change, run install + plugin re-enable BEFORE testing, committing, or manual verification:
  ```
  python3 -m pip install -e . --break-system-packages && hive plugin enable code-review && hive plugin enable cvim && hive plugin enable fork && hive plugin enable notify
  ```
  This is a single mandatory step. Do not skip it. Do not split it. Do not "do it later".
- Why this matters: plugin commands under `~/.factory/commands/` are materialized copies, not symlinks, so changing plugin code without re-enabling can leave you testing stale command files.
- `PYTHONPATH=src python -m pytest tests/ -q` — run the full test suite.
- `PYTHONPATH=src python -m pytest tests/ -m unit -q` — fast unit tests only.
- `PYTHONPATH=src python -m pytest tests/ -m cli -q` — CLI-layer tests.
- `PYTHONPATH=src python -m pytest tests/ -m e2e -q` — end-to-end tmux tests.
- `PYTHONPATH=src python -m pytest tests/unit/test_cvim_command.py tests/unit/test_cvim_payload.py -q` — focused `/cvim` and `/vim` sendback coverage.

## Coding Style & Naming Conventions

Use Python 3.11+ with 4-space indentation and type hints where practical. Match the existing style: small focused functions, minimal comments, and straightforward dataclass-based models. File names are lowercase with underscores. Test names should be explicit, e.g. `test_wait_status_times_out_without_match`.

## Testing Guidelines

Every CLI command should have at least one CLI test and complex flows should also have e2e coverage. Add unit tests for pure logic before relying on higher-level tests. Keep new tests in the correct layer and use shared fixtures from `tests/conftest.py` or helpers in `tests/e2e/_helpers.py`.

When touching `/cvim` popup sendback behavior, keep `tests/unit/test_cvim_command.py::test_popup_schedules_post_after_popup_exits` passing. It guards the regression where `run-shell` was started before popup teardown completed, causing the returned edit payload to be swallowed.

## Commit & Pull Request Guidelines

Follow the existing history style: short conventional messages such as `fix: ...`, `refactor: ...`, or `docs: ...`. Keep commits scoped to one logical change. Before opening a PR, run the relevant pytest targets, summarize the behavioral change, and call out tmux/droid assumptions or manual verification steps.

## Security & Runtime Notes

Do not hardcode secrets, session IDs, or local machine paths. Hive depends on `tmux` and Factory `droid`; e2e tests assume tmux is available and use a fake droid binary for isolation.
