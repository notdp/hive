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

Codex override: a daemon-backed (born-connected) codex pane reports `busy`
from its per-pane app-server instead — both branches above are still computed
but then replaced for that pane. See "Codex Native Runtime (app-server source)".

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
`@hive-cli` tag, a surviving codex app-server daemon/thread, or
transcript/session metadata — all of those outlive the CLI process. Probe
failures fail closed to `false`.

Meaning — the member's CLI process is actually running. Spawned launches do
not `exec` over the pane shell, so the pane (and `alive`) survives its CLI
exiting; the retained shell is not an agent runtime. The three states:

| state | `alive` | `cliAlive` | `inputState` | `inputReason` | `busy` |
|---|---|---|---|---|---|
| pane dead | false | false | offline | pane_dead | false |
| retained shell (CLI exited) | true | false | offline | cli_exited | false |
| anchored member (`remote`) | true | false | offline | cli_exited | false |
| live CLI | true | true | per runtime | per runtime | per runtime |

Consumers — delivery refuses a retained shell before any native transport
(the send event stays durable on the bus); idle notify, session-snapshot
capture, and duo pairing all skip retained shells.

### `remote`

Source — the member pane's `@hive-remote` tag, written at registration.

Meaning — the member's agent process does not live on this pane. `channel`
is the only value today: the pane is an **anchor** whose channel socket and
ready marker are symlinks to an external Claude session's own
`hive-client-<pid>.sock`. The pane exists to hold the member's identity, so
every pane-keyed authority (routing, tags, `kill-pane` as the kick control,
doctor) keeps working unchanged.

The honest asymmetry: `alive` no longer implies the member is reachable, and
`cliAlive` is permanently `false` because no CLI was ever meant to run here.

**An anchored member has no push transport.** A host that launches its
sessions without `--channels` — the desktop app owns its argv — cannot
receive channel notifications at all, so nothing is pushed to this pane and
the channel socket is never written to. Delivery is the durable bus write
plus the member's own inbox hook, which drains it after each tool call and
again at the end of the turn. `Agent.send` names that boundary
`busInboxAccepted`; it deliberately claims less than the channel and daemon
classifications, because no push was attempted and none may be claimed.

The socket still earns its keep as **liveness evidence**: the external
session's channel server unlinks the real socket and marker when it exits, so
the anchor's symlinks dangle and delivery fails closed exactly like a dead
pane server — the message stays on the bus for its return.

Consumers — delivery gates an anchored member on that liveness rather than
refusing it as a retained shell; the resume snapshot records the marker and
resume skips those members instead of spawning a look-alike CLI on the team's
routing key (an external session reconnects itself by re-running `hive duo
init --channel <socket>`, which relinks the anchor). Because every session on
the host runs the same inbox hook, the saved context carries a `session`
claim: the first session to drain owns the identity and siblings are refused,
and re-forming the duo clears the claim.

### `inputState`

Source:

- transcript gate inspection via `check_input_gate()`
- codex app-server `status.activeFlags` for a daemon-backed codex pane
  (overrides the transcript gate for that pane — see "Codex Native Runtime")

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

Codex has no transcript/JCL probe. A daemon-backed pane reports natively (see
"Codex Native Runtime" below); an embedded (daemon-less) codex is unsupported
and reads as `unknown_evidence`.

## Codex Native Runtime (app-server source)

A born-connected codex pane — hive-spawned, or launched through `hive codex` /
the `hive shell-init` shell function — runs a per-pane `codex app-server`
daemon. Hive connects as a second client over that pane's unix socket and reads
`busy` / `inputState` / `turnPhase` **natively** from the daemon's
notification stream, instead of reverse-engineering them from the transcript.
The emitted payload is tagged `_runtimeSource: codex_app_server`.

This path is taken only when a live per-pane daemon answers. An embedded
(manually launched, non-daemon) codex has no socket and is deliberately
unsupported **as a Hive team member**: `hive init` / `hive duo` reject it at
team entry, and a team-bound `hive fork` / `hive handoff --fork` refuses to
clone a codex pane (`codex fork` would launch embedded). A standalone embedded
codex still runs, but hive reads no state from it — session id stays
`unresolved`, `turnPhase` stays unknown, and there is no transcript fallback.

State is event-sourced from app-server notifications and stays valid until the
next event — there is no time-based staleness gate. The relevant notifications
are `thread/status/changed`, `turn/started`, and `turn/completed`.

Field mapping (notification → runtime field):

- `busy`
  - `true` — `turn/started`, or `thread/status/changed` with `status.type=active`
  - `false` — `turn/completed`, or `status.type=idle`
- `turnPhase`
  - `tool_open` — any `active` turn. The native path does not subdivide active
    phases (no `tool_result_pending_reply` / `user_prompt_pending` split); it
    trades transcript-tail granularity for an authoritative busy edge.
  - `turn_closed` — `idle` / `turn/completed`
  - `unknown_evidence` — before the first event; `notLoaded` / `systemError`
    leave the prior phase unchanged
- `inputState`
  - `waiting_user` — `active` whose `status.activeFlags` contain
    `waitingOnApproval` or `waitingOnUserInput`; emitted with
    `inputReason=app_server_active_flag`
  - `ready` — any other `active`, or `idle`

`sessionId` for a daemon-backed pane resolves from app-server thread metadata
(`thread.sessionId` via `thread/resume`), with an lsof-on-daemon-pid fallback.
It stays `unresolved` until the thread has produced activity.

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
