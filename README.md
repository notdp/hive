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
hive doctor ──→ diagnose agent connectivity
workspace ────→ hive.db (SQLite) + artifacts/ + sidecar daemon
```

## Install

Requires: Python 3.11+, tmux, at least one agent CLI (`claude`, `codex`, or `droid`)

```bash
pipx install git+https://github.com/notdp/hive.git
pipx upgrade hive   # update to latest
npx skills add https://github.com/notdp/hive -g --skill hive --agent '*' -y
```

Use `npx skills add` as the canonical installation path for the base `hive` skill. `hive plugin enable ...` installs plugin commands and plugin-owned skills such as `code-review`; it does not update the repo-root `skills/hive/SKILL.md`.

For local development against your current checkout, install the skill from the repo path instead of GitHub:

```bash
npx skills add "$PWD" -g --skill hive --agent '*' -y
```

## Quick Start

```bash
# Inside tmux, bind the current window as a team
hive init
hive team
hive peer show

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

# Diagnose connectivity
hive doctor
hive doctor dodo
hive doctor --skills

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
| `hive team` / `hive teams` | Show team with runtime inputState/activity and peer info, or list teams |
| `hive peer show\|set\|clear` | Inspect or persist default peer pairs |
| `hive send <agent> "text"` | Send message (fire-and-forget with delivery tracking) |
| `hive answer <agent> "text"` | Answer a pending AskUserQuestion |
| `hive doctor [agent] [--skills]` | Diagnose agent connectivity and optional local hive skill drift |
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
├── hive.db         # SQLite: messages + observations
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

Immediate `hive send` states:

- `queued`: accepted and now tracked in the background; continue working
- `pending`: submit completed and background tracking continues; continue working
- `confirmed`: delivery was confirmed in the initial send window
- `failed`: local submit failed before tracking began; retry

## Plugins

```bash
hive plugin enable cvim      # edit previous message as diff
hive plugin enable notify    # human notification popup
hive plugin enable fork      # vfork/hfork shortcuts
hive plugin enable code-review  # multi-agent code review skill
```

Plugin helpers (`cvim`, `vim`, `vfork`, `hfork`) are for the **human**, not the model. In Claude Code / Codex, use `!hive cvim` via shell escape. Plugin enable does not install or refresh the base `hive` skill; use `npx skills add ... --skill hive` for that.

## Development

```bash
python3 -m pip install -e . --break-system-packages
npx skills add "$PWD" -g --skill hive --agent '*' -y
hive plugin enable code-review && hive plugin enable cvim && hive plugin enable fork && hive plugin enable notify

PYTHONPATH=src python -m pytest tests/ -q
```

If a local code change affects sidecar-backed behavior, do not trust an already-running sidecar during manual verification. After the install + skill refresh + plugin re-enable step, stop the current workspace sidecar first, then rerun the verification command so it starts a fresh daemon from the updated code. This applies to checks like `hive doctor`, delivery tracking, `hive activity`, and other sidecar-derived runtime fields.

## License

MIT
