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
hive reply ──────→ reply to task messages and publish projected state
workspace ───────→ artifacts/ + events/ for durable coordination
```

## Install

Requires: Python 3.11+, tmux, [droid](https://docs.factory.ai)

```bash
pipx install git+https://github.com/notdp/hive.git

# Update to latest
pipx upgrade hive
```

## Quick Start

```bash
# Inside tmux, start one or more agent panes first
hive current

# Bind the current tmux window as a Hive team
hive init
hive team

# Send work to another pane and inspect projected progress
hive send dodo "Review the staged diff and write findings to an artifact"
hive reply orch "review complete" --state done --artifact /tmp/review.md
hive team   # inspect projected progress via the `statuses` field

# Fork the current agent into a new split pane (auto-picks direction)
hive fork              # or: hive vfork / hive hfork

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
| `hive team` / `hive teams` | Show the current team, including projected status in `statuses`, or list known teams |
| `hive spawn <agent>` | Spawn a new agent pane |
| `hive send <agent> "text"` / `hive reply <agent> "text"` | Deliver structured `<HIVE ...>` messages and publish projected state |
| `hive capture <agent>` / `hive interrupt <agent>` | Inspect or interrupt an agent pane |
| `hive exec <terminal> "cmd"` / `hive terminal ...` | Drive registered terminal panes |
| `hive plugin enable|disable|list` | Materialize first-party plugin scripts under `~/.factory/commands/` and link plugin skills |
| `hive fork` | Fork the current agent session into a new split pane (auto-detects best direction) |
| `hive cvim` / `hive vim` | Pop an external editor; send a structured diff (`cvim`) or a blank draft (`vim`) back to the current agent pane |
| `hive vfork` / `hive hfork` | Shorthand for `hive fork -s v` / `hive fork -s h` (runs in background, sends Escape to the source pane) |
| `hive notify "message"` | Notify the human attached to the current pane |
| `hive delete <team>` | Kill agents and remove team data |

## Workspace

When created with `--workspace`, hive initializes a workspace for durable workflow state and large artifacts:

```
workspace/
├── state/          # Shared key-value state files
├── presence/       # Team presence snapshots from `hive team`
├── events/         # Append-only Hive message/event log
└── artifacts/      # Large payloads exchanged by path
```

## Environment Variables

| Variable | Description |
|----------|-------------|
| `HIVE_TEAM_NAME` | Default team name for commands that support implicit team resolution |
| `HIVE_AGENT_NAME` | Agent name assigned to spawned panes |
| `HIVE_HOME` | Hive data directory (default: `~/.hive`) |

## Plugins

First-party plugins carry **human-only helper scripts** (`cvim`, `vim`, `vfork`, `hfork`, `notify`) and real model skills (`code-review`) that Hive materializes on `hive plugin enable`:

```bash
hive plugin list
hive plugin enable cvim
hive plugin enable notify
hive plugin disable cvim
```

### How the human invokes plugin helper commands

The plugin bash helpers live in `~/.hive/plugins/installed/<plugin>/commands/`. They are intended to be triggered **by the human**, not by the model. Hive exposes the same underlying scripts through two entry points:

- **Factory / Droid** — `hive plugin enable` copies each helper into `~/.factory/commands/<name>`. Droid loads it as a native slash command, so the human types `/cvim`, `/vim`, `/vfork`, `/hfork`, or `/notify` in the Droid input box to run the underlying bash script.
- **Claude Code and Codex** — no slash command or `.md` wrapper is installed. Instead, the human types the command inline via the agent's shell escape (`!` prefix):
  ```
  !hive cvim          # edit the previous assistant message as a diff
  !hive cvim -1       # edit the message two turns back (index grows backwards)
  !hive vim           # open a blank buffer and send the result back
  !hive vfork         # fork the current session into a vertical split
  !hive notify "build finished"
  ```
  The `hive cvim` / `hive vim` / `hive vfork` / `hive hfork` subcommands forward to the same materialized plugin helpers, so the editor popup, paste-back, and tmux split behavior are identical to Droid's `/cvim` flow.

**These helpers are not exposed to the model.** There is no slash command, skill, or tool-call surface that lets Claude / Codex / Droid invoke `hive cvim` on their own — the shell escape `!hive cvim` is a human gesture in the agent's input box. The model should never be "taught" to call them.

Plugin model skills (e.g. `code-review`) are still symlinked into `~/.factory/skills/`, `~/.claude/skills/`, and `~/.codex/skills/` from `~/.hive/plugins/installed/`.

Re-running `hive plugin enable ...` is required whenever you change plugin command code locally, because the Factory commands directory contains materialized copies rather than symlinks.

## Local Development

```bash
# Editable install
python3 -m pip install -e . --break-system-packages

# After local plugin changes, refresh materialized command copies and installed plugin bundles
hive plugin enable code-review && hive plugin enable cvim && hive plugin enable fork && hive plugin enable notify

# Full test suite
PYTHONPATH=src python -m pytest tests/ -q

# Focused cvim regression coverage
PYTHONPATH=src python -m pytest tests/unit/test_cvim_command.py tests/unit/test_cvim_payload.py -q
```

## How It Works

Hive runs interactive `droid`/`claude`/`codex` sessions in tmux panes. Short coordination messages arrive inline as `<HIVE ...>` blocks via tmux `send_keys`; durable coordination lives in workspace `events/` and `artifacts/`, while projected collaboration state is surfaced in `hive team` under the `statuses` field. No JSON-RPC, no daemon — just tmux + workspace files.

Each spawned agent is a full `droid` TUI session. You can `tmux select-pane` to interact with any agent directly.

## License

MIT
