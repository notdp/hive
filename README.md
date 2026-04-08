# Hive

tmux-based multi-agent collaboration framework for [Factory](https://factory.ai)'s `droid` CLI.

Spawn multiple droid agents in tmux panes, orchestrate them via CLI, inject short control messages inline via tmux, and persist workflow state in a workspace.

## Architecture

```
tmux window
┌──────────────┬──────────────┬──────────────┐
│  orch (you)  │    agent     │   terminal   │
└──────────────┴──────────────┴──────────────┘

hive current/init ─→ discover or bind the current tmux window
hive spawn/fork ──→ add more agent panes when needed
hive send ───────→ inject inline <HIVE ...> messages via tmux
hive status-set ─→ publish per-agent state snapshots
workspace ───────→ artifacts/ + status/ for durable coordination
```

## Install

Requires: Python 3.11+, tmux, [droid](https://docs.factory.ai)

```bash
pipx install git+https://github.com/notdp/hive.git
```

## Quick Start

```bash
# Inside tmux, start one or more agent panes first
hive current

# Bind the current tmux window as a Hive team
hive init
hive team

# Send work to another pane and publish progress
hive send dodo "Review the staged diff and write findings to an artifact"
hive status-set busy "investigating cvim popup sendback"
hive status

# Bring the human back only when needed
hive notify "修复完成了，按 Space 回来确认"
```

If you prefer to start from the CLI instead of binding an existing tmux window:

```bash
hive create my-team -d "code review" --workspace /tmp/hive-demo
hive spawn claude -t my-team -m "custom:claude-opus-4-6"
hive spawn codex -t my-team -m "custom:gpt-5.4"
hive send claude "Review the PR diff and write findings to the workspace artifact"
```

## Commands

| Command | Description |
|---------|-------------|
| `hive current` | Inspect the current tmux/Hive binding and get the next-step hint |
| `hive init` / `hive create <team>` | Bind the current tmux window or create a fresh team |
| `hive team` / `hive teams` | Show the current team or list known teams |
| `hive spawn <agent>` | Spawn a new agent pane |
| `hive send <agent> "text"` | Deliver an inline `<HIVE ...>` message to another member |
| `hive status` / `hive status-set` / `hive wait-status` | Publish and observe collaboration state |
| `hive capture <agent>` / `hive interrupt <agent>` | Inspect or interrupt an agent pane |
| `hive exec <terminal> "cmd"` / `hive terminal ...` | Drive registered terminal panes |
| `hive plugin enable|disable|list` | Materialize first-party Factory commands and skills |
| `hive notify "message"` | Notify the human attached to the current pane |
| `hive delete <team>` | Kill agents and remove team data |

## Workspace

When created with `--workspace`, hive initializes a workspace for durable workflow state and large artifacts:

```
workspace/
├── state/          # Shared key-value state files
├── presence/       # Team presence snapshots from `hive who` / `hive status`
├── status/         # Per-agent workflow status snapshots
└── artifacts/      # Large payloads exchanged by path
```

## Environment Variables

| Variable | Description |
|----------|-------------|
| `HIVE_TEAM_NAME` | Default team name for commands that support implicit team resolution |
| `HIVE_AGENT_NAME` | Agent name assigned to spawned panes |
| `HIVE_HOME` | Hive data directory (default: `~/.hive`) |

## Plugins

First-party plugins install Factory-facing commands and skills such as `/cvim`, `/vim`, `/notify`, and `code-review`:

```bash
hive plugin list
hive plugin enable cvim
hive plugin enable notify
hive plugin disable cvim
```

Command files under `~/.factory/commands/` are materialized copies, while plugin skills are symlinked from `~/.hive/plugins/installed/`. Re-running `hive plugin enable ...` is therefore required whenever you change plugin command code locally.

## Local Development

```bash
# Editable install
python3 -m pip install -e . --break-system-packages

# After local plugin changes, refresh materialized Factory commands and installed plugin bundles
hive plugin enable code-review && hive plugin enable cvim && hive plugin enable fork && hive plugin enable notify

# Full test suite
PYTHONPATH=src python -m pytest tests/ -q

# Focused cvim regression coverage
PYTHONPATH=src python -m pytest tests/unit/test_cvim_command.py tests/unit/test_cvim_payload.py -q
```

## How It Works

Hive runs interactive `droid`/`claude`/`codex` sessions in tmux panes. Short coordination messages arrive inline as `<HIVE ...>` blocks via tmux `send_keys`; long payloads and durable completion signals live in workspace `artifacts/` and `status/`. No JSON-RPC, no daemon — just tmux + workspace files.

Each spawned agent is a full `droid` TUI session. You can `tmux select-pane` to interact with any agent directly.

## License

MIT
