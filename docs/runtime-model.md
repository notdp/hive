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

## Runtime Field Reference

Every CLI's runtime now comes from a **native source** — the runtime the CLI
itself maintains — never from screen scraping or transcript tail heuristics:

- claude: the session registry entry its bg-job engine writes
  (`_runtimeSource: claude_bg`) — see "Claude Native Runtime"
- codex: the shared app-server daemon's status stream
  (`_runtimeSource: codex_app_server`) — see "Codex Native Runtime"
- grok: the per-pane leader's notification stream
  (`_runtimeSource: grok-leader`) — see "Grok Native Runtime"

The tmux control-mode output monitor remains only as the fallback `busy`
heuristic for panes with no native state (terminal panes, unmanaged CLIs) and
as the idle-notify target-pane chooser.

### `busy`

Source — the pane's native runtime source, when one holds state for it:

1. codex: shared daemon `thread/status/changed`
2. grok: leader `activity` notifications
3. claude: the session registry `status` field (`busy` / `idle` / `waiting`),
   read from the bg engine's entry (via the pane's job record) or, for an
   interactive non-member claude on the pane tty, from that session's own
   entry

Fallback (no native state) — tmux control-mode output within the last `3s`,
gated by transcript jsonl mtime advance within the same window (the
phantom-redraw gate that suppresses Ink/ratatui frame-redraw spikes).
If the transcript path can't be resolved, the fallback returns true on
monitor activity alone — idle-notify must never silently disappear for panes
the gate can't introspect.

Combined into ``sidecar._pane_is_truly_busy``.

### `cliAlive`

Meaning — the member's agent runtime is actually alive. Spawned launches do
not `exec` over the pane shell, so the pane (and `alive`) survives; a
retained shell is not an agent runtime.

Source, per CLI:

- codex / grok: live process evidence on the pane's TTY only — the pane's
  current command and TTY process table, parsed by the shared CLI matchers
  (`agent_cli.detect_cli_process_for_pane`). Never the pane title, the
  `@hive-cli` tag, or a surviving daemon.
- claude: the bg job's engine state, **never** the pane TTY — the pane only
  shows an attach viewer, and a viewer gap (reattach window, closed viewer)
  is not member death. See the three-tier liveness table under "Claude
  Native Runtime".

The generic three states:

| state | `alive` | `cliAlive` | `inputState` | `inputReason` | `busy` |
|---|---|---|---|---|---|
| pane dead | false | false | offline | pane_dead | false |
| retained shell (CLI exited) | true | false | offline | cli_exited | false |
| live CLI | true | true | per runtime | per runtime | per runtime |

Consumers — delivery refuses a dead runtime before any native transport (the
send event stays durable on the bus); idle notify and duo pairing skip dead
runtimes.

### `inputState`

Source:

- claude: registry `status == waiting` plus its `waitingFor` label
  (`inputReason: registry:<waitingFor>`)
- codex: app-server `status.activeFlags` (see "Codex Native Runtime")
- grok: leader `session/request_permission` (see "Grok Native Runtime")
- unmanaged panes: transcript gate inspection via `check_input_gate()`

Current values:

- `ready`
- `waiting_user`
- `unknown`
- `offline`

Meaning:

- whether the agent is currently waiting for a user answer

Important consumer:

- the send gate: `hive send` reads the member's runtime payload and refuses
  while the target is `waiting_user` — with one waiver, claude's
  `registry:dialog open` (a `/status`-style dialog in an attached viewer
  parks the status on waiting, but the inbox still queues normally)

### `turnPhase`

Source:

- codex app-server thread status for a daemon-backed codex pane
- grok leader notifications for a daemon-backed grok pane
- claude emits **no** `turnPhase`: the registry `status` carries no turn
  structure, and the transcript tail probe that used to synthesize one is
  retired

Current values:

- `tool_open`
- `turn_closed`
- `input_backlog`
- `tool_result_pending_reply`
- `user_prompt_pending`
- `unknown_evidence`

Meaning:

- the phase the receiver's turn is in, per its daemon's events
- consumers treat an absent `turnPhase` as "no turn structure available" and
  fall back to `busy` / `_runtimeSource` (duo pairing does exactly this)

## Claude Native Runtime (bg job source)

A hive claude member is a **`claude --bg` job**. The engine — a full Claude
Code TUI on a pty owned by claude's own supervisor daemon (argv `claude
bg-spare`) — runs outside tmux; the member's pane only shows it through a
`claude attach <jobId>` viewer held in the managed launcher's watch loop.
The pane process table therefore says nothing about the member's life: the
viewer is furniture, the job is the member.

Identity is the **jobId** — durable across engine restarts, wakes and
upgrades (the engine pid is not; the sessionId is durable too and stays the
resume/transcript key). Which job belongs to which tmux pane is a per-pane
`hive-pane-<n>.job` record under `<claude-config>/hive-control/`, written at
spawn / managed-launch time (the same shape as codex's `.thread` records).
`Agent.session_id` for a claude member IS its jobId, and resume snapshots
carry it. Tool-side identity: the engine's tool subprocesses carry
`CLAUDE_CODE_MESSAGING_SOCKET=/tmp/cc-socks/<enginePid>.sock`; hive parses
the engine pid out of it, reads that engine's registry entry for the jobId,
and reverse-looks-up the pane through the job records (the claude analogue of
`CODEX_THREAD_ID`). This resolution also satisfies the in-tmux gate for the
engine's tools, whose env has no usable `$TMUX`.

Signal surfaces:

- `<claude-config>/sessions/<enginePid>.json` — the live engine's registry
  entry: `kind:"bg"`, `jobId`, `status` (`idle`/`busy`/`waiting`, an observed
  vocabulary, not a documented enum), `waitingFor` (only while waiting),
  `statusUpdatedAt`, `sessionId`, `messagingSocketPath`. The attach viewer
  never registers. This entry is the busy/inputState/delivery authority; a
  `statusUpdatedAt` older than 30 minutes demotes the status to `unknown`
  (`inputReason: stale_status`) without touching liveness.
- `claude agents --json --all` — the durable job ledger. Consulted only when
  the engine entry is missing (~270ms per call, cached ~30s in the sidecar);
  the ledger's `state` field lags reality and is never used for liveness.
- `jobs/<jobId>/state.json` is deliberately **not** read (undocumented
  fields).

Three-tier liveness:

| tier | evidence | meaning | payload |
|---|---|---|---|
| alive | engine registry entry present (pid live, socket exists) | engine up; `status` is truth | `cliAlive: true`, status-mapped fields |
| asleep | no entry, but the ledger lists the jobId (row without pid/status) | supervisor parked the job (~1h idle) or it was stopped; **not dead** — wake revives it with the same jobId/sessionId; never reaped | `cliAlive: true`, `busy: false`, `inputState: ready`, `_engineState: asleep` |
| gone | no entry and no ledger row | job removed | `cliAlive: false`, `inputState: offline`, `inputReason: engine_gone` |

Status mapping (registry `status` → runtime fields):

- `busy` → `busy: true`, `inputState: ready`
- `idle` → `busy: false`, `inputState: ready`
- `waiting` → `busy: false`, `inputState: waiting_user`,
  `inputReason: registry:<waitingFor>`

Delivery self-heals through the wake primitive: `Agent.send` resolves pane →
job record → engine entry → inbox socket; when the entry is missing but the
ledger still lists the job, a tty-less `claude attach <jobId>` (stdin at
/dev/null) revives the engine — new pid, same jobId/sessionId — and delivery
re-reads the fresh entry. Only a job missing from the ledger (or a failed
wake) is a `DeliveryError`. Spawn readiness is the engine's registry entry
appearing, proven **before** the pane command is even typed; spawn env is
washed of `CLAUDE*`/`ANTHROPIC*` vars (an inherited
`CLAUDE_CODE_CHILD_SESSION` makes the engine skip registration entirely), and
path-valued spawn flags must be absolute (they persist verbatim as the job's
`respawnFlags`).

The managed launcher (`hive claude`, the `hclaude` path) makes user launches
the same shape: an interactive launch mints a bg job (flags and prompt ride
the spawn), `--resume <jobId>` rebinds the pane and wakes a parked job (what
spawn panes, resume hints and `hive resume` all run), `-r <sessionId>
[--fork-session]` mints a bg job resuming/forking that session, and
management subcommands / headless / picker shapes pass through to plain
claude. The pane then sits in an attach watch loop: `claude attach` exits 0
both on user detach and when an engine respawn/upgrade kicks the viewer, so
the loop reattaches after a 1s window the user can break with Ctrl-C; a
non-zero attach (job removed) ends the loop.

Lifecycle: `hive kill` (and team cleanup) parks the member's job with
`claude stop` before killing the pane — the job stays in the ledger and
`hive resume` can wake it. The sidecar's claude supervisor tick prunes job
records whose pane died and parks those orphaned engines the same way; it
never reattaches viewers (the watch loop self-heals, and a user who left it
deliberately must not be typed at) and never touches an asleep member with a
live pane.

Keyboard paths (`hive inject`, claude `/compact`, cvim sendback) still work:
keystrokes pass through the attach viewer into the engine pty. Each checks
for a live claude process on the pane tty first and refuses during the
viewer gap rather than typing into the pane shell.

An unmanaged claude — a bare interactive TUI with no job record — is
deliberately unsupported **as a Hive team member**: `hive init` / `hive duo`
reject it at team entry, and delivery to a recorded-less claude pane fails
loudly. It still works as a `ccd.<name>` guest session over its own inbox.

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
