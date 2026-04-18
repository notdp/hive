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
pipx upgrade hive   # update the Hive CLI
npx skills add https://github.com/notdp/hive -g --all
npx skills update hive -g   # refresh the globally installed hive skill (only works for github-sourced installs)
```

Use `npx skills add` as the canonical installation path for the base `hive` skill. `hive plugin enable ...` installs plugin commands and plugin-owned skills such as `code-review`; it does not update the repo-root `skills/hive/SKILL.md`.
Upgrading the CLI does not refresh the installed `hive` skill automatically. When the skill is stale, `hive` commands run from an agent pane warn on stderr, and `hive doctor --skills` shows the exact mismatch.

For local development against your current checkout, install the skill from the repo path instead of GitHub. The skills CLI does not record a lock entry for local sources, so `npx skills update` cannot refresh this install — rerun the same `npx skills add` command to pick up changes:

```bash
npx skills add "$PWD" -g --all
```

## Quick Start

```bash
# Inside tmux, bind the current window as a team
hive init
hive team

# Send a root message: short summary in body, details in artifact
cat <<'EOF' | hive send dodo "review the staged diff" --artifact -
diff summary and review context
EOF

# Send with artifact
hive send orch "done" --artifact /tmp/review.md

# Hand off the latest unanswered inbound thread to another agent
hive handoff dodo --artifact /tmp/task.md

# Pipe stdin as artifact (preferred for large content)
cat <<'EOF' | hive send orch "see report" --artifact -
# Findings
- item
EOF

# Reply (auto-picks the latest unanswered inbound from orch)
hive reply orch "fixed"

# Reply to a specific earlier thread when auto-pick would target the wrong one
hive reply orch --reply-to aBc1 "fixed"

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
| `hive team` | Show team with runtime `busy` / `inputState` and peer info |
| `hive peer set\|clear` | Persist or clear default peer pairs |
| `hive send <agent> "text"` | Start a new thread (root send only; artifact required; auto-forks a clone when target is in an active turn) |
| `hive reply <agent> "text"` | Reply on an existing thread (auto-picks latest unanswered inbound; `--reply-to` for explicit msgId) |
| `hive handoff <agent>` | Delegate a thread via direct send, spawn, or fork wrapper |
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
| `--artifact <path>` | Attach a file (required on root sends) |
| `--artifact -` | Read artifact from stdin |
| `--wait` | Block until transcript confirms delivery |

### Reply options

| Option | Description |
|--------|-------------|
| `--reply-to <msgId>` | Override the auto-resolved thread anchor |
| `--artifact <path>` | Attach a file |
| `--artifact -` | Read artifact from stdin |
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

A send is *delivered* when the target pane's output shows the message `msgId` (either via Claude transcript JSONL or the tmux control-mode stream for the target pane). `hive send` runs a short grace window in-process; if it doesn't settle immediately:

- A team-scoped **sidecar daemon** tracks it for up to 60 seconds in the background
- Results land as observation events in `hive.db` with `confirmationSource = transcript | stream` (on success)
- Failed deliveries are injected back into the sender pane as `<HIVE-SYSTEM>` exceptions

Root sends without `--reply-to` must keep `body` to a short summary and put detailed context in `artifact` (prefer `--artifact -`; only use a file path when you already have one). Hive currently enforces this by rejecting root bodies that are longer than `500` chars, have `3+` lines, contain fenced code (`````), or start markdown heading/list lines (`# ` / `- ` / `* `). When the target member is in an active turn (transcript still shows `tool_open` / `user_prompt_pending` / `tool_result_pending_reply`, or `busy=True` with any non-closed `turnPhase`), the root send path auto-forks a clone pane so the new thread runs without interrupting the original; the response payload carries `routingMode=fork_handoff` and `routingReason=active_turn_fork`.

`hive send` response carries a `delivery` field:

- `success`: target pane rendered msgId; delivery confirmed
- `pending`: submit completed; background tracking continues (up to 60s)
- `failed`: submit errored OR target pane never rendered msgId before timeout

**Shell quoting footgun (applies to both `hive send` and `hive reply`)**: double-quoted `body` strings with backticks get pre-processed by zsh/bash as command substitution, and the message is silently rewritten before Hive sees it. For anything containing markdown inline code, prefer heredoc + `--artifact -`, or wrap the body in single quotes.

## Plugins

```bash
hive plugin enable cvim      # edit previous message as diff
hive plugin enable notify    # human notification popup
hive plugin enable fork      # vfork/hfork shortcuts
hive plugin enable code-review  # multi-agent code review skill
```

Plugin helpers (`cvim`, `vim`, `vfork`, `hfork`) are for the **human**, not the model. In Claude Code / Codex, use `!hive cvim` via shell escape. Plugin enable does not install or refresh the base `hive` skill; use `npx skills add <source> -g --all` for that.

## Development

```bash
python3 -m pip install -e . --break-system-packages
npx skills add "$PWD" -g --all
hive plugin enable code-review && hive plugin enable cvim && hive plugin enable fork && hive plugin enable notify

PYTHONPATH=src python -m pytest tests/ -q
```

If a local code change affects sidecar-backed behavior, do not trust an already-running sidecar during manual verification. After the install + skill refresh + plugin re-enable step, stop the current workspace sidecar first, then rerun the verification command so it starts a fresh daemon from the updated code. This applies to checks like `hive doctor`, delivery tracking, and other sidecar-derived runtime fields.

For the current runtime-field model and fork-gate semantics, see
`docs/runtime-model.md`.

## License

MIT
