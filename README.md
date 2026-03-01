# Hive

tmux-based multi-agent collaboration framework for [Factory](https://factory.ai)'s `droid` CLI.

Spawn multiple droid agents in tmux panes, orchestrate them via CLI, and communicate through the filesystem.

## Architecture

```
┌──────────────┬──────────────┐
│              │   agent-1    │
│ orchestrator ├──────────────┤
│   (you)      │   agent-2    │
└──────────────┴──────────────┘

orchestrator ──hive spawn──→ tmux split-window → droid TUI
orchestrator ──hive type───→ send_keys → agent stdin
orchestrator ──hive capture─→ capture_pane → agent stdout
agents ────────filesystem───→ workspace/tasks/ & workspace/results/
```

## Install

Requires: Python 3.11+, tmux, [droid](https://docs.factory.ai)

```bash
pipx install git+https://github.com/notdp/hive.git
```

## Usage

```bash
# Create a team (from inside tmux)
hive create my-team -d "code review"

# Spawn agents
hive spawn claude -t my-team -m "custom:claude-opus-4-6"
hive spawn gpt -t my-team -m "custom:gpt-5.3-codex"

# Send a task
hive type claude "Review the PR diff and write findings to /tmp/results.md" -t my-team

# Monitor
hive status -t my-team
hive capture claude -t my-team

# Interrupt / cleanup
hive interrupt claude -t my-team
hive delete my-team
```

## Commands

| Command | Description |
|---------|-------------|
| `hive create <team>` | Create a team + optional workspace |
| `hive spawn <agent>` | Spawn a droid agent in a new tmux pane |
| `hive type <agent> "text"` | Send a prompt to an agent |
| `hive capture <agent>` | Read agent's pane output |
| `hive status` | Show team and agent status (JSON) |
| `hive interrupt <agent>` | Press Escape in agent's pane |
| `hive wait <agent> <tag>` | Poll until sentinel file appears |
| `hive comment` | GitHub PR comment operations |
| `hive delete <team>` | Kill agents + remove team data |

## Workspace

When created with `--workspace`, hive initializes a filesystem workspace for structured agent communication:

```
workspace/
├── state/          # Key-value state files
├── tasks/          # Task files written by orchestrator
├── results/        # Result files + .done sentinels from agents
└── comments/       # GitHub comment tracking
```

## Environment Variables

| Variable | Description |
|----------|-------------|
| `HIVE_TEAM_NAME` | Default team name (auto-set for spawned agents) |
| `HIVE_AGENT_NAME` | Agent's own name (auto-set on spawn) |
| `HIVE_HOME` | Data directory (default: `~/.hive`) |

## How It Works

Hive takes the same approach as Claude Code's Agent Teams: interactive TUI agents in tmux panes, controlled via `send_keys`, communicating through the filesystem. No JSON-RPC, no daemon — just tmux + files.

Each spawned agent is a full `droid` TUI session. You can `tmux select-pane` to interact with any agent directly.

## License

MIT
