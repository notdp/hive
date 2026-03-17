# Repository Guidelines

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

- `pip install -e .` — install Hive in editable mode.
- `PYTHONPATH=src python -m pytest tests/ -q` — run the full test suite.
- `PYTHONPATH=src python -m pytest tests/ -m unit -q` — fast unit tests only.
- `PYTHONPATH=src python -m pytest tests/ -m cli -q` — CLI-layer tests.
- `PYTHONPATH=src python -m pytest tests/ -m e2e -q` — end-to-end tmux tests.

## Coding Style & Naming Conventions

Use Python 3.11+ with 4-space indentation and type hints where practical. Match the existing style: small focused functions, minimal comments, and straightforward dataclass-based models. File names are lowercase with underscores. Test names should be explicit, e.g. `test_wait_status_times_out_without_match`.

## Testing Guidelines

Every CLI command should have at least one CLI test and complex flows should also have e2e coverage. Add unit tests for pure logic before relying on higher-level tests. Keep new tests in the correct layer and use shared fixtures from `tests/conftest.py` or helpers in `tests/e2e/_helpers.py`.

## Commit & Pull Request Guidelines

Follow the existing history style: short conventional messages such as `fix: ...`, `refactor: ...`, or `docs: ...`. Keep commits scoped to one logical change. Before opening a PR, run the relevant pytest targets, summarize the behavioral change, and call out tmux/droid assumptions or manual verification steps.

## Security & Runtime Notes

Do not hardcode secrets, session IDs, or local machine paths. Hive depends on `tmux` and Factory `droid`; e2e tests assume tmux is available and use a fake droid binary for isolation.
