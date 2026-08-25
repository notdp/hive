# Hive Runtime Model

This document records the current runtime design that Hive actually implements.
It is intentionally narrower than a full architecture spec. The goal is to pin
down the meanings, sources, and intended uses of the runtime fields and
active-turn fork routing that already exist in code.

## Scope

This document covers:

- `busy`
- `inputState`
- `turnPhase`
- root-message summary/artifact protocol
- active-turn fork routing

This document does not define:

- a semantic global `busy/idle` truth model
- automatic scheduling
- automatic fork/spawn decisions
- automatic garbage collection

For the raw Claude transcript structures that feed these runtime decisions,
see `docs/transcript-signals.md`.

## Runtime Layers

Hive now exposes two different runtime layers on purpose:

1. Output activity layer (`busy`)
2. Turn phase layer (`turnPhase`)

They answer different questions and should not be conflated.

### Output Activity Layer

Field:

- `busy: true | false`

Question answered:

- Has this pane produced tmux-visible output in the last 3 seconds, and
  is that output corroborated by recent transcript jsonl mtime advance?

What it is good for:

- lightweight live activity display
- knowing whether a pane is currently emitting output

What it is not:

- not a semantic "agent is definitely busy"
- not a safe-to-interrupt truth value

### Turn Phase Layer

Field:

- `turnPhase: <token>`

Question answered:

- What phase of a turn does the receiver's transcript tail currently show?

What it is good for:

- deciding whether to fork the target or direct-send
- explaining why Hive treated that target as it did

What it is not:

- not the same thing as pane output activity
- not the same thing as a universal busy/idle truth model

## Runtime Field Reference

### `busy`

Source — `busy=true` when **either** of two branches holds:

1. **Output branch** — tmux control-mode output stream
   (`tmux.ControlModeOutputMonitor`) reported visible output within the
   last `3s`, AND the agent transcript jsonl mtime advanced within the
   same window. The mtime check is a phantom-redraw gate that suppresses
   TUI frame-redraw spikes (Ink / ratatui re-printing on-screen characters
   during idle).
2. **Active-turn branch** — transcript ``turnPhase`` ∈
   :data:`activity.ACTIVE_TURN_PHASES` (``tool_open`` /
   ``tool_result_pending_reply`` / ``user_prompt_pending`` /
   ``input_backlog``). This branch catches the streaming-gap case where
   tmux visible-text payloads space out beyond `3s` mid-tool, and it
   bypasses the output branch's gates: an agent in mid-turn is busy
   regardless of monitor activity or transcript mtime.

Combined into ``sidecar._pane_is_truly_busy``.

Native-daemon override: a hive-managed codex pane (recorded thread on the
shared app-server daemon) or a daemon-backed grok pane (per-pane leader)
reports `busy` from its native daemon transport instead — both branches above
are still computed but then replaced for that pane. See "Codex Native Runtime
(app-server source)" and "Grok Native Runtime (leader source)".

Fail-open: if the transcript path can't be resolved (non-agent pane, no
session yet, stat error), the output branch returns true on monitor
activity alone — idle-notify must never silently disappear for panes the
gate can't introspect.

Notes:

- the active-turn branch is what makes idle-notify safe under streaming
  agents (Claude/Codex tool loops): the public `busy` field tracks
  "agent in mid-turn", not just "tmux output in the last 3s"
- known limitation: a CLI that emits visible output for longer than the
  threshold without writing the transcript jsonl AND whose `turnPhase`
  the probe can't recognise can be gated as a false negative; the
  threshold is intentionally conservative

### `cliAlive`

Source — live process evidence on the pane's TTY only: the pane's current
command and its TTY process table, parsed by the shared CLI matchers
(`agent_cli.detect_cli_process_for_pane`). Never the pane title, the
`@hive-cli` tag, a surviving codex app-server or grok leader daemon, or
transcript/session metadata — all of those outlive the CLI process. Probe
failures fail closed to `false`.

Meaning — the member's CLI process is actually running. Spawned launches do
not `exec` over the pane shell, so the pane (and `alive`) survives its CLI
exiting; the retained shell is not an agent runtime. The three states:

| state | `alive` | `cliAlive` | `inputState` | `inputReason` | `busy` |
|---|---|---|---|---|---|
| pane dead | false | false | offline | pane_dead | false |
| retained shell (CLI exited) | true | false | offline | cli_exited | false |
| live CLI | true | true | per runtime | per runtime | per runtime |

Consumers — delivery refuses a retained shell before any native transport
(the send event stays durable on the bus); idle notify, session-snapshot
capture, and duo pairing all skip retained shells.

### `inputState`

Source:

- transcript gate inspection via `check_input_gate()`
- codex app-server `status.activeFlags` for a daemon-backed codex pane
  (overrides the transcript gate for that pane — see "Codex Native Runtime")
- grok leader `session/request_permission` for a daemon-backed grok pane
  (see "Grok Native Runtime")

Current values:

- `ready`
- `waiting_user`
- `unknown`
- `offline`

Meaning:

- whether the agent is currently waiting for a user answer

Important consumer:

- the send gate (`hive send` refuses while the target is `waiting_user`)

### `turnPhase`

Source:

- transcript probe for claude (last observed transcript state)
- codex app-server thread status for a daemon-backed codex pane — codex has no
  transcript probe (see "Codex Native Runtime")
- grok leader notifications for a daemon-backed grok pane — grok has no
  transcript probe either (see "Grok Native Runtime")

Current values:

- `tool_open`
- `turn_closed`
- `input_backlog`
- `tool_result_pending_reply`
- `user_prompt_pending`
- `assistant_text_idle`
- `unknown_evidence`

Meaning:

- the phase the receiver's turn is in, as seen in the transcript tail
- consumers pick the subsets they care about (see "Consumer Subsets" below)

## Hard Busy vs Turn Phase

These are related, but they are not the same concept.

### Hard Busy

`hard busy` is a reasoning concept, not a public field. It means:

- a tool/task open event has happened
- the corresponding close event has not happened yet

Example:

- Claude: `tool_use` without matching `tool_result`

In `turnPhase` terms, hard busy surfaces as `tool_open`. `input_backlog` is a
strategy-level non-open state that also matters to consumers but is not hard
busy.

Hard busy is not currently surfaced as its own public runtime field.

## Current CLI-Specific Evidence

Each row maps a transcript/JCL observation to the emitted `turnPhase` value.

### Claude

- `tool_open` — `tool_use` open
- `input_backlog` — unresolved queue backlog is the newest decisive evidence
- `turn_closed` — `turn_duration` or `stop_hook_summary` with `preventedContinuation=false`
- `tool_result_pending_reply` — tool result arrived but assistant has not clearly continued
- `user_prompt_pending` — real user prompt pending
- `assistant_text_idle` — assistant text without stronger closing/opening evidence

### Codex

Codex has no transcript/JCL probe. A hive-managed pane (recorded thread on the
shared daemon) reports natively (see "Codex Native Runtime" below); an
unmanaged codex — embedded, or remote but never recorded — is unsupported and
reads as `unknown_evidence`.

### Grok

Grok has no transcript/JCL probe either. A daemon-backed pane reports natively
(see "Grok Native Runtime" below); a grok hive never spawned has no leader
socket and no session record, and reads as `unknown_evidence`.

## Codex Native Runtime (app-server source)

One `codex app-server` daemon per CODEX_HOME (socket
`$CODEX_HOME/app-server-control/hive-shared.sock`) hosts every hive codex
thread. Each codex TUI attaches to its own thread with `codex resume
<threadId> --remote unix://<sock> --cd <cwd>`; hive connects as one more
client over the same socket and reads `busy` / `inputState` / `turnPhase`
**natively** from the daemon's status stream, instead of reverse-engineering
them from the transcript. The emitted payload is tagged `_runtimeSource:
codex_app_server`.

Identity is the threadId (== transcript sessionId), never the process env: the
daemon's env is frozen at spawn time and shared by every thread (hive strips
`TMUX_PANE` from it), and codex injects the thread's own `CODEX_THREAD_ID`
into tool subprocesses. Which thread belongs to which tmux pane is a per-pane
`.thread` record beside the socket, written at spawn / managed-launch time;
`hive` invocations inside a codex tool resolve their pane by reverse lookup of
`CODEX_THREAD_ID` through those records.

Spawn mints the thread up front: hive calls `thread/start` (with cwd, and the
model when pinned) followed by `thread/name/set` — the name write flushes the
rollout to disk, without which a fresh thread is not resumable — records the
pane binding, and launches the TUI as a `resume` of that thread. `hive codex
fork <sid>` forks server-side (`thread/fork`), records the fork, and resumes
it the same way. Directory trust in remote mode is judged from the daemon's
config.toml on disk, so every new cwd gets `[projects."<dir>"] trust_level =
"trusted"` written before its thread starts.

This path is taken only when the pane has a recorded thread and the shared
daemon answers. An unmanaged codex — embedded, or a `resume` picker launch
whose chosen thread hive cannot know — is deliberately unsupported **as a Hive
team member**: `hive init` / `hive duo` reject it at team entry. It still
runs, but hive reads no state from it — session id stays `unresolved`,
`turnPhase` stays unknown, and there is no transcript fallback.

The daemon is machine-level shared state: hive never kills it (a dead daemon
takes every attached TUI down with it within ~5s), and the sidecar supervises
it — a dead daemon is respawned while the team has live codex members, and a
member pane whose CLI exited but whose thread is recorded gets one `hive codex
resume <threadId>` typed into its retained shell (guarded by a live-process
check, a shell-prompt check, and a cooldown). Records of dead panes are
pruned on the same tick.

State is event-sourced from the daemon's broadcasts and stays valid until the
next event — there is no time-based staleness gate. On a shared daemon a
non-turn-owning client receives only `thread/status/changed` (and
`thread/goal/*`); `turn/*` and `item/*` go to the turn's own client, so status
events are the sole busy source. A client that connected after a thread went
active backfills its current status once via `thread/resume`.

Field mapping (notification → runtime field):

- `busy`
  - `true` — `thread/status/changed` with `status.type=active`
  - `false` — `status.type=idle`
- `turnPhase`
  - `tool_open` — any `active` status. The native path does not subdivide
    active phases (no `tool_result_pending_reply` / `user_prompt_pending`
    split); it trades transcript-tail granularity for an authoritative busy
    edge.
  - `turn_closed` — `idle`
  - `unknown_evidence` — before the first event; `notLoaded` / `systemError`
    leave the prior phase unchanged
- `inputState`
  - `waiting_user` — `active` whose `status.activeFlags` contain
    `waitingOnApproval` or `waitingOnUserInput`; emitted with
    `inputReason=app_server_active_flag`
  - `ready` — any other `active`, or `idle`

`sessionId` for a hive-managed pane IS the recorded threadId — a plain record
read, available from spawn time with no probing and no `unresolved` window.

## Grok Native Runtime (leader source)

A born-connected grok pane — hive-spawned, or launched through `hgrok` (the
`hive shell-init` launcher) — runs a per-pane `grok agent leader` daemon. The
TUI attaches to it, and hive attaches as a second client through `grok agent
--leader stdio`, an ACP JSON-RPC subprocess. `busy` / `inputState` / `turnPhase`
are folded from that client's notification stream; the emitted payload is tagged
`_runtimeSource: grok-leader`.

The leader keeps every session of the cwd, so which one is *this pane's* is not
discoverable from it: hive mints the session id at spawn time, passes it as
`--session-id`, and records it beside the socket. The client loads exactly that
session and ignores notifications for any other.

`session/load` replays the session's past updates before it answers, so
everything received before the load response is discarded — a replayed turn must
never mark the pane busy. This is why spawn asks the sidecar to connect
(`connect-grok`) once the pane's session exists and its grok is up, rather than
lazily on the next tick.

Field mapping (notification → runtime field):

- `busy` — `_x.ai/sessions/changed` `activity` is the authority:
  - `true` — `activity: working`, or any `session/update` chunk/tool event
  - `false` — `activity: idle`, or `_x.ai/session_notification` `turn_completed`
- `turnPhase`
  - `tool_open` — `session/update` `tool_call`
  - `tool_result_pending_reply` — `tool_call_update` with `status: completed`
  - `user_prompt_pending` — an agent/thought/user message chunk with no tool
    phase open
  - `input_backlog` — `_x.ai/queue/changed` with non-empty entries
  - `turn_closed` — `turn_completed`, or `activity: idle`
  - `unknown_evidence` — before the first post-load notification
- `inputState`
  - `waiting_user` — the leader asked `session/request_permission`; hive answers
    its copy `cancelled` (the decision belongs to the human at the TUI, which
    gets its own copy) and emits `inputReason=leader_permission_request`
  - `ready` — `turn_completed`, `activity: idle`, or any `tool_call_update` (the
    permission it was blocked on has been decided)

Queue semantics: a prompt sent mid-turn is queued FIFO by the leader and runs
when the current turn ends — there is no steering and no bounce, the same as
typing into the TUI. Delivery is therefore accepted at the *echo* (a queue entry
or `user_message_chunk` carrying the text), not at the `session/prompt`
response, which only lands when the whole turn ends.

`sessionId` for a daemon-backed pane is the spawn-minted id read straight from
the pane's `.session` file — no probing, and no `unresolved` window while the
session warms up.

## Root Send Protocol

Root sends are every `hive send`; the command no longer accepts
`--reply-to`. Continuing an existing thread is done via `hive reply`
(which always carries a `replyTo` and is therefore not subject to the
root protocol).

Hive enforces a two-layer protocol for root sends:

- `body`: short summary only
- `artifact`: detailed content

Current root-body hard failures:

- body longer than `500` chars
- body with `3+` lines
- body containing fenced code
- body lines starting with markdown heading/list markers:
  - `# `
  - `- `
  - `* `

This rule applies to root sends. Replies are not subject to these summary-body
limits.

## Why There Is Only One Runtime Doc

This design was split many times during discussion, but the stable part that
actually shipped is small enough to keep in one place:

- output activity
- interrupt safety
- root protocol
- active-turn fork routing

Keeping these together reduces drift between overlapping docs.
