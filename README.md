# Hive

tmux-based multi-agent collaboration runtime for CLI agents (`claude`, `codex`, `droid`).

Agents run in tmux panes, communicate via inline `<HIVE>` messages, and coordinate through a shared SQLite store with a team-scoped sidecar daemon for delivery tracking.

## Architecture

```
tmux window
┌──────────────┬──────────────┬──────────────┐
│  lead pane   │  peer agent  │  terminal*   │
└──────────────┴──────────────┴──────────────┘

hive init ────→ bind current tmux window as a team
hive send ────→ inject <HIVE msgId=... > message, track delivery
hive answer ──→ answer a pending AskUserQuestion
hive inbox ───→ check unread messages + delivery observations
hive doctor ──→ diagnose agent connectivity
workspace ────→ hive.db (SQLite) + artifacts/ + sidecar daemon
```

## Install

Requires: Python 3.11+, tmux, at least one agent CLI (`claude`, `codex`, or `droid`)

```bash
pipx install git+https://github.com/notdp/hive.git
pipx upgrade hive   # update to latest
```

## Quick Start

```bash
# Inside tmux, bind the current window as a team
hive init
hive team

# Send a message (fire-and-forget, delivery tracked by sidecar)
hive send dodo "review the staged diff"

# Send with artifact
hive send orch "done" --artifact /tmp/review.md

# Pipe stdin as artifact (preferred for large content)
printf '%s\n' "# Findings" "- item" | hive send orch "see report" --artifact -

# Reply to a specific message
hive send orch "fixed" --reply-to aBc1

# Answer a pending question
hive answer dodo "yes"

# Check unread messages
hive inbox
hive inbox --peek  # inspect without advancing the cursor

# Diagnose connectivity
hive doctor
hive doctor dodo

# Fork session into a new split
hive fork

# Notify the human
hive notify "done, press Space to come back"
```

## Commands

| Command | Description |
|---------|-------------|
| `hive current` | Inspect current tmux/Hive binding |
| `hive init` / `hive create` | Bind current window or create a team |
| `hive team` / `hive teams` | Show team with runtime inputState, or list teams |
| `hive send <agent> "text"` | Send message (fire-and-forget with delivery tracking) |
| `hive answer <agent> "text"` | Answer a pending AskUserQuestion |
| `hive inbox` / `hive inbox --peek` | View unread messages and mark as read / inspect without marking |
| `hive doctor [agent]` | Diagnose agent connectivity |
| `hive spawn <agent>` | Spawn a new agent pane |
| `hive fork` | Fork current session into a new split |
| `hive notify "msg"` | Notify the human on the current pane |
| `hive delete <team>` | Remove team (workspace preserved by default) |
| `hive plugin enable\|disable\|list` | Manage plugins |

### Send options

| Option | Description |
|--------|-------------|
| `--artifact <path>` | Attach a file |
| `--artifact -` | Read artifact from stdin |
| `--reply-to <msgId>` | Link to a previous message |
| `--wait` | Block until transcript confirms delivery |

## Workspace

```
workspace/
├── hive.db         # SQLite: messages, observations, cursors
├── artifacts/      # Large payloads exchanged by path
├── state/          # Shared key-value state files
└── run/            # Sidecar socket and runtime files
```

## Delivery Tracking

`hive send` uses a 1-second grace window to confirm delivery in-process. If the message isn't confirmed immediately:

- A team-scoped **sidecar daemon** tracks it in the background
- The sidecar detects CLI queue state (transcript or tmux capture)
- Results land as observation events in `hive.db`
- High-value exceptions (`unconfirmed`, `tracking_lost`) are injected back to the sender pane

The sender doesn't need to do anything — `send` is fire-and-forget.

## Plugins

```bash
hive plugin enable cvim      # edit previous message as diff
hive plugin enable notify    # human notification popup
hive plugin enable fork      # vfork/hfork shortcuts
hive plugin enable code-review  # multi-agent code review skill
```

Plugin helpers (`cvim`, `vim`, `vfork`, `hfork`) are for the **human**, not the model. In Claude Code / Codex, use `!hive cvim` via shell escape.

## Development

```bash
python3 -m pip install -e . --break-system-packages
hive plugin enable code-review && hive plugin enable cvim && hive plugin enable fork && hive plugin enable notify

PYTHONPATH=src python -m pytest tests/ -q
```

## License

MIT
