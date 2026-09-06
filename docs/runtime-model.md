# Hive runtime model

Where each fact about a running member comes from, which layer owns which
truth, and what the delivery transports do with a message once it reaches an
engine.

Boundary with [transcript-view.md](transcript-view.md): this document owns
what is true of a running member — the state its engine reports and what its
transport does with a message. transcript-view.md owns what a read-only
observer can recover from a session's JSONL, which is strictly less: the
transcript holds only what was written to it, and a delivery folded into a
running turn writes no turn of its own.

## Out of scope by decision

Hive does not define a semantic global `busy`/`idle` truth, automatic
scheduling, automatic fork/spawn decisions, or automatic garbage collection.
It reports what each engine reports about itself and does not synthesize a
scheduler on top. Scheduling around a busy target belongs to the receiver's
queue.

## The registry truth layer and the tmux display layer

A team exists because it has a registry entry, not because a window renders
it. tmux is a display layer resolved on top: window options and pane tags
record where the team is rendered, not whether it exists. Deleting the window
removes the display; `hive delete` ends the team.

Consequences across modules:

- **Roster writers.** The CLI adds and removes roster names; the hived only
  backfills fields of names already there. An observation racing a `hive kill`
  must not resurrect the killed member.
- **The `display` window id.** It is a cache: authority checks do not read
  it, and hived identity is `(workspace socket, team)`, so a dead window does
  not retire a hived on its own; a missing registry entry with no window left
  behind it does.
- **One directory per team.** `$HIVE_HOME/teams/<team>/` holds everything
  hive owns for the team: `team.json` (the entry — present means the team
  exists), and, on the default workspace, `hive.db` (the bus), `run/`
  (`hived.sock`, `notify.jsonl`, `hived.stderr`, `cvim/`) and `artifacts/`.
  `--workspace <DIR>` on `create` puts the workspace elsewhere and the
  entry's `workspace` field records it; `team.json` stays in the team
  directory. `create` always resets the default workspace (a pool name
  recycled after `hive delete` must not inherit the old bus or event log).
  `hive delete` removes `team.json` and leaves the rest; `--down` first
  retires every member and kills the team's tmux session;
  `--delete-workspace` removes the whole directory (or the external
  workspace); an external workspace is never removed without the flag. A
  long `HIVE_HOME` relocates the hived socket under `/tmp/hive-<uid>/` as
  any long workspace does (`devlog.rs::hived_socket_path_in`).
- **Verbs outside tmux.** The team verbs (create/join/spawn/team/kill/
  delete/attach) need no tmux client: `create` outside tmux puts the
  team window in the session named after the team (created detached when
  missing), `spawn` splits
  a pane into the team's window by id from anywhere, and `attach` rebuilds
  a window that is gone before jumping to it. A pane serves as an address;
  these verbs do not require the caller to have one. `node run` rides the
  same doctrine; `view`, the read-only listings (`ls`, `ccd`), `worktree`, and
  the setup/launcher commands never needed a pane. The full list is
  `cli/mod.rs::TMUX_OPTIONAL_ROOT_COMMANDS`; everything else (layout,
  fork, inject, cvim, …) acts on the current pane and refuses to run outside
  tmux, except `send`, which is admitted when the caller's own session names
  a roster row or a Claude messaging socket (the ladder in the next
  bullet).
- **Engine key scope.** A member's daemon is keyed by `<team>.<member>`, so it
  survives the pane; a raw non-team pane keeps a pane key and pane lifecycle.
  With no pane to ask, an engine still resolves who it is, by a three-rung
  ladder: the pane's own tags, then the roster row keyed by the session id
  the engine mints for itself and exports to its own tool subprocesses (a
  `CODEX_THREAD_ID` or a `GROK_SESSION_ID`, each matched only against rows of
  that cli; a Claude messaging socket, through the session it names), then
  the saved context file. Hive hands an engine no identity of its own in env,
  and no rung reads one: a variable hive sets is inherited, not minted, and
  Claude's machine-level bg supervisor daemon freezes the launch env of
  whichever `claude --bg` first started it and hands that env to every engine
  it forks afterwards, so a member could arrive carrying a stranger's name.
  Each spawn env is washed of the *other* engines' markers for the same
  reason. The first rung that resolves settles the identity, including when
  it names a different team; an engine whose session matches no row is
  nobody, and `hive send` tells it so rather than letting it sign as the
  orch. Display is then resolved on top of identity, not read from env: a
  member engine's tools carry no usable `TMUX_PANE` (a claude bg engine has
  none, codex's daemon env is frozen at spawn, a grok member's leader is
  minted before any pane exists and pins none), so `identity::current_pane_id`
  walks from the engine's own marker to its pane — a codex thread to its
  pane record, a claude socket to its engine's job to the job's pane, a grok
  session id to its roster row to the pane tagged with that member. That is
  what lets the pane-bound verbs (layout, fork, inject, cvim, …) run from a
  member's tool shell; a member whose pane is gone keeps its identity and
  loses only those verbs. A raw `hive grok` pane outside any team is the one
  leader that still pins its pane.
- **Reaping on failed reads.** Daemon reaping does not fire on an unreadable
  registry read, and a young pidfile gets a grace window so a spawn
  mid-registration is not mistaken for an orphan.
- **Team name allocation.** A name-pool pick skips every name the registry
  still lists, so no create lane reuses a name until `hive delete` releases
  it.

The display is eager and never defines membership: every create leaves the
team bound to a window (the caller's inside tmux, a fresh one in the team
session outside), every spawn splits a pane into it, and `hive attach` heals
it, rebuilding a window that is gone and adding a pane for any roster member
without one. Only a member with a recorded engine identity and an attachable
cli gets a pane; the rest are named on stderr when the window is rebuilt, and
stay registry-only until they have one. A window hive built itself carries
`@hive-built`; `hive delete` closes those and leaves a window a human's
session lent the team (an in-tmux create). A claude member whose sessionId names an interactive session (a
creating or joined desktop/ccd session, not a bg job) is drawn read-only
through `hive view`, because the resume lane would mint a forked job that
steals the member's deliveries. That mirror is an ordinary pane
tagged `@hive-role mirror` beside its member tags: the first pane of a team
window. The mirror is display state only.

hive owns the team window's layout, and that too is display state. The
planner (`layout/plan.rs`) reads the window's size and its panes' roles
and emits one tmux layout string: the mirror is a left column in a
landscape window (`w >= 2h`) and a top row in a portrait one, half the
window unless the members score better beside an 80-column / 24-row
mirror; the members take the grid whose equal cells come closest to
80x24. `select-layout` hands cells to panes in window order, so the apply
swaps the mirror first before it lands. The key of the plan last applied —
orientation, member count, mirror presence, grid, mirror share, never
absolute sizes — sits on the window as `@hive-layout`. Two window
hooks (`window-resized`, `window-layout-changed`, installed wherever a
window is marked as hive's) run `hive layout auto --on-change --window`,
which re-plans and applies only when the key differs (the apply's flock is
keyed on the window id, so the hook's `@N` and a verb's `session:index`
serialize on one file, and only a human's `hive layout auto` waits for it —
the hook form and the explicit call sites yield to an apply in flight,
leaving a rerun marker the holder consumes with one more plan, so a drag
that fires the hook per step never queues processes; a window down to
one pane drops its key, so the next member is planned): a client attaching
at another size, a spawn, a kill, a mirror coming and going all
re-plan without a hived, while a human's border drag — same key — holds,
through proportional resizes, until the plan changes. `hive layout auto`
from a human forces the apply; an explicit preset applies as given and
holds the same way. `hive delete` (and every tag sweep) unsets the hooks
and the key with the window tags: a window a human's session lent the
team is theirs again, not re-tiled at their next split.
`@hive-mirror` on the window is the recorded choice: `off`, written by
`hive mirror off`, keeps heal and backfill from drawing it; `on`, written by
`hive mirror on` or when a session mirror is built, is what makes the status
bar's orch chip appear; unset reads as open — nothing withholds the mirror
by default. `hive mirror off` parks the pane with `break-pane -d` in a hidden
window of the team session (the caller's session when the team has none)
tagged `@hive-hidden <team>`: the viewer keeps running, every team-window
scan masks that window (`#{?@hive-hidden,,#{@hive-team}}` — a window format
reads the parked pane's `@hive-team` through), and `hive delete` closes it.
`on` joins the same pane back as the first pane with `join-pane -b`, or
rebuilds it the way a heal would when the parked pane is gone; a heal or
backfill that finds a parked pane of the member joins that one rather than
starting a second viewer. `off` refuses when the mirror is the window's only
pane, because `break-pane` on a lone pane renames the window in place. The
status bar's orch chip and `prefix+m` run the same verb with `--window`: a
`run-shell` job carries no `TMUX_PANE`.

The team session hive builds — `hive create` outside tmux, `hive attach`
rebuilding a lost window — carries hive's own two-line
status bar, installed by session id at build (`tmux/status.rs`; `status*`
are session options, so a window a human's session lent the team gets none
and the human's global status is untouched). Its colours follow the
viewer's appearance switch — `view.theme`, `HIVE_VIEW_THEME`, then
detection (`view_theme.rs`), resolved once at install, so a theme change
shows at the next session build — and the bar is rendered from tmux options
alone, with no `#()` in the format: `@hive-team`; `@hive-mirror` (orch chip,
▴ parked / ▾ open, absent while unset); per pane `@hive-role`, `@hive-agent`,
`@hive-busy`, `@hive-unread` and `@hive-notify-active` (✱, the attention
mark); `@hive-pr`; on the second line `@hive-notify-text`, then
`@hive-ticker`. The chips are `range=pane|<id>` click targets and the orch
chip a `range=user|hive-mirror` one (the install also sets the session's
`mouse on`, so clicks reach them whatever the global setting); the root
`MouseDown1Status` binding
installed with the bar routes them to `select-pane -t =` and `hive mirror
--window`, and falls through to tmux's stock `select-window -t =` for every
other status line. The `prefix+m` binding installed with it is gated on
`@hive-team` the same way: its else branch is the command the key ran
before hive bound it (`list-keys -T prefix m` at install, kept in the
server option `@hive-prefix-m` so a later install behind hive's own binding
still has it), so a non-team window keeps tmux's `select-pane -m` or the
human's binding. `@hive-busy`, `@hive-unread` and `@hive-ticker` are the
hived's status tick (`hived/status.rs`), written as edges and only to
agent-role panes: busy is the same `is_output_busy` verdict idle-notify
uses; unread is a send the hived accepted for the pane and has not seen it
busy since; the ticker is the two newest bus sends as `from → to · age ·
"first words"`, `#` doubled because the status line draws an option value
verbatim. They are display of the runtime fields below, never a source for
them.

### Addresses beyond the roster

Of the send address kinds, only a member names an engine with a transport.
`ccd.<name>` reaches a Claude session outside any team over that session's
own inbox. A `hive node run` dispatch has no reply address at all: the
member is never asked to send anything back, and the roster holds engines
only.

### Node dispatch: the result is the turn's final message

`hive node run --team T --name N [--cli C] [--model M]` runs one task on a
member the way a Claude Code Workflow runs a subagent: the member is told
nothing about replying, does the task, ends its turn, and the runner
(`node.rs`) reads the final assistant message of that turn from the engine's
own transcript. Nothing travels back over the bus.

- **The dispatch.** The runner mints a dispatch id `nd-<12 lowercase hex>`,
  writes the task to `<workspace>/artifacts/tasks/<name>-<dispatch_id>.md`,
  and injects an envelope with no `from`:
  `<HIVE to=<team>.<name> artifact=<that path>>`, body `task <dispatch_id>`
  followed by the task's first line, `</HIVE>`. The id therefore appears
  verbatim in the text the member receives (header and body). The ledger row
  has `from_agent` empty (there is no sender), `to_agent` the member name and
  `artifact` the task path; it is the only bus write a node makes. The run
  record (below) is written `pending` — dispatch id, engine session, reader
  cursor — before that write, so a runner that dies between the delivery
  and its own bookkeeping leaves a pending record behind, never a gap a
  same-name run could walk through.
- **Readiness.** The runner dispatches only between turns, and only on a
  positive reading from the engine's own daemon that no turn is open. The
  runner asks the hived's `turn-open` for the member and the hived queries
  the engine directly, with no tick cache in between (codex: the
  app-server's `thread/read`; claude: the bg job's engine record, whose
  `busy` flag is no answer once its status is stale; grok: the leader
  pool's push-fed state). No answer says nothing about the turn and never
  opens the dispatch. A member still in a turn after 600s ends the
  run `member_busy` without dispatching — a task dropped into a running
  turn would be folded into it, and the fold detection below is a net,
  not a plan.
- **Anchoring is input identity, never time.** Before dispatching, the
  runner takes the reader's cursor (where the transcript ends now). After
  dispatching it asks the reader for the input record carrying the id past
  that cursor, binds the turn that input started, and waits for that turn's
  terminal record. A turn the reader cannot attribute to the id is reported,
  not guessed at: an id that landed inside a running turn instead of
  opening one, a second input folded into the bound turn after it opened
  (a human typing into the pane, a teammate's message absorbed mid-turn —
  the two fold paths are detected separately), a compaction that rewrote
  the branch — all `ambiguous`. A member whose engine session is no longer
  the one the dispatch landed in (`/clear`, a resume into another id, a
  fork) is `session_changed`: the old transcript by itself cannot show that
  a new file was opened, so the core re-reads the member's engine session
  while it waits and voids the anchor when it moved, after one last read of
  the old transcript for a terminal record already there. The readers
  (`adapters/turn.rs`, one per CLI) are the only place transcript turn and
  terminal shapes are interpreted; `node.rs` sees `TurnOutcome` values and
  nothing else.
- **A closed turn whose text is not on disk yet.** Engines close a turn
  before its final message is fully written: grok writes `turn_ended`
  before the history line, claude writes a message one content block per
  record. A reader in that state returns `TurnOutcome::Flushing`, which is
  not a result: the core keeps polling under a 30s flush budget from the
  first `Flushing` reading and ends `ambiguous` when the text never lands.
  Earlier text of the same turn (a tool-calling step's narration, a block
  of an earlier message) never stands in for the final message.
- **The result.** `body` is every text block of the bound turn's final
  assistant message, original order, thinking and tool blocks excluded, not
  truncated to a sentence or a line; it is empty when the turn closed
  without a text block. A completed turn says the engine closed it, not that
  the task succeeded: a refusal or a request for help is a normal final
  message.
- **The JSON line** (stdout, exit 0 whenever a verdict was reached): `status`,
  `name`, `pane` (may be empty), `reused`, `dispatchId`, `session` (the
  engine's own session id — for claude the session uuid the bg job runs,
  never the job id the roster row holds), `turn` (engine turn key, or
  null), `body` when `status` is `completed`, `reason` (the engine's own
  label or an explanation) otherwise. `status` is one of `completed |
  interrupted | failed | ambiguous | session_changed |
  transcript_unavailable | member_gone | member_busy`: the first five are
  the same-named `TurnOutcome`; `transcript_unavailable` is a reader that
  kept failing to read for 60s after the dispatch, or a roster row that
  never got a session id; `member_gone` is a member the roster reports dead
  with no outcome readable on one final read; `member_busy` is a pending
  node record for the member whose member is alive, the per-member lock
  held by another runner, or the 600s readiness cap above. Polling is 1s,
  the reader is re-invoked on every poll, and a half-written trailing line
  is "not yet"; the turn itself has no timeout (the caller decides).
  stderr and exit 1 mean the task was not dispatched — bad team, spawn or
  ready failure, no reader for the cli — and the run can be repeated
  (`member_busy` is the other not-dispatched verdict, reported as a JSON
  line because it names a state the caller acts on); a dispatched task
  always ends in a JSON line.
- **The record.** `<workspace>/run/nodes/<name>.json` — `dispatchId`,
  `cli`, `session`, `cursor`, `anchor` (session/turn/cursor once bound),
  `status` (`pending | input_bound | <terminal status>`), `body`/`reason`
  when terminal, `seq` (ledger seq of the dispatch, filled in after the
  delivery), `startedAt` (epoch seconds) — under the flock
  `<workspace>/run/nodes/<name>.lock` held for the whole run; the lock
  file itself is never deleted, the record is. A stale pending record whose
  member is dead is replaced by the next run; `hive kill` of the member
  removes its record. A same-name node reuses a live member. Two v1
  limitations: Ctrl-C on the runner leaves the record pending until
  `hive kill` of the member, and a terminal verdict frees the name even
  though the member may still be working (an `ambiguous` or
  `transcript_unavailable` run releases the lock while the engine can be
  mid-task, so the next same-name run dispatches into whatever the member
  is doing and its fold detection is the net).

#### Node turn anchors, per CLI

What each reader binds and waits for, in the engine's own records. Every
reader re-opens the transcript on every call, holds no state between calls,
and treats a trailing line without its newline as not written yet.

- **claude** — transcript `~/.claude/projects/<cwd slug>/<sessionId>.jsonl`
  (the slug is the cwd with every non-alphanumeric character replaced by
  `-`; the session id is the engine's uuid, resolved from the roster's job
  id through the job's engine record). The input is the `user` record whose
  `message.content` (a string or text blocks) carries the dispatch id;
  a `tool_result` row and a row flagged `isMeta` or `turnCompanion` (a
  harness companion) are never inputs. Between turns the daemon lane writes
  it as `promptSource: queued` after a `queue-operation enqueue`/`dequeue`.
  The turn key is that record's `uuid`; the turn is the records whose
  `parentUuid` chains back to it, and because claude repairs its own chain
  through records it never wrote (a refusal fallback, an interrupt), a
  chained record after the anchor whose parent nobody since the anchor
  wrote is adopted as the running turn's. The terminal record is an
  assistant record on the turn with `message.stop_reason == "end_turn"`.
  Claude writes one API message as one record per content block, every
  block carrying the message's `stop_reason` and `message.id`, and the
  thinking block with `end_turn` lands before the text block that follows
  it — so the final message is the text blocks of the records sharing that
  `message.id`, complete only once a barrier has landed after them: a
  record with a `uuid` that is not one of its blocks, or one of the
  uuid-less rows claude writes only after a turn closes (`last-prompt`, the
  `dequeue` that opens the next queued turn). Until then the reader is
  `Flushing`. A `user` record `[Request interrupted by user…]` (Escape, or
  a rejected tool call) is `interrupted`; an `isApiErrorMessage` assistant
  record, `stop_reason` `max_tokens` or `refusal`, and a `system
  model_refusal_no_fallback` are `failed`. `ambiguous`: at dispatch, the id
  landing only in a `queued_command` attachment and a terminal
  `queue-operation remove` (folded into a turn already running — see "What
  a delivery leaves in the receiver's transcript"); after binding, a
  `queued_command` attachment or an absorbed `remove` inside the turn (a
  second input folded in, whose text may have redirected the member), a
  fresh `user` input chained into or started outside the turn, a
  `compact_boundary` or an `isCompactSummary` row. A replaced or truncated
  file, or a record from another `sessionId`, is `session_changed`.
- **codex** — rollout `~/.codex/sessions/**/rollout-*-<threadId>.jsonl`.
  Every turn is bracketed by `event_msg` records: `task_started
  {turn_id}`, the turn's `response_item`s, one terminal event for the same
  `turn_id`. The input is the `response_item` user message whose
  `input_text` carries the id; the turn key is the `turn_id` of the
  `task_started` open at that point, not the message's own metadata. An id
  that lands with no `task_started` since the cursor, or into an open turn
  that has already written engine output (an assistant message, a tool
  call or its output), was steered into a running turn: `ambiguous`.
  Codex's own user-role injections before the id inside the same turn
  (`<environment_context>`, a skill expansion) do not disqualify it. The
  terminal record is `task_complete` with the bound `turn_id`: the final
  message is its `last_agent_message`, else the turn's last `final_answer`
  assistant message, else empty (commentary never stands in). `turn_aborted`
  (`reason: interrupted`; older builds omit the `turn_id`) is
  `interrupted`; `error {message, codex_error_info}`, which carries no
  `turn_id` and in every sample precedes a null `task_complete`, is
  `failed`. A `task_complete` or `task_started` for another `turn_id`
  before the bound turn closed, a `thread_rolled_back`, or another user
  message inside the turn is `ambiguous`; a replaced or shortened rollout,
  or a `session_meta` naming another id, is `session_changed`.
- **grok** — two files under
  `~/.grok/sessions/<urlencode(cwd, safe='')>/<sessionId>/`.
  `chat_history.jsonl`: the input is the `user` record whose text carries
  the id and that has a `prompt_index` — the same coordinate as the turn
  number; a mid-turn user record (a `<system-reminder>`, an interjection
  folded into the running turn) carries `synthetic_reason` and no
  `prompt_index`, and an id in one of those is `ambiguous`. `events.jsonl`:
  `turn_started` carries `session_id` and `turn_number`, `turn_ended`
  carries `outcome` (and `cancellation_category` for a cancel) but no turn
  number, so the reader binds the `turn_started` whose `turn_number` is the
  prompt's `prompt_index` (its `session_id` must be the member's) and walks
  the events after it in order: the first `turn_ended` closes the turn,
  another `turn_started` first or an `interjected` event is `ambiguous`.
  `conversation_message_count` is never used. The turn key is
  `<session_id>/<turn_number>`. On `outcome: completed` the final message is
  the last `assistant` record of the turn's history span (up to the next
  prompt) that carries no `tool_calls` — a tool-calling record is a step and
  its narration is not the answer. `turn_ended` lands before that record is
  flushed, so a completed turn with no such record yet is `Flushing` under
  the flush budget, and empty once a later prompt or `turn_started` shows
  nothing is coming. `cancelled` is `interrupted` with the category,
  `error` is `failed`, any other outcome is `ambiguous`; a session
  directory gone or events from another `session_id` is `session_changed`.

## Runtime fields and their sources

Every field comes from the CLI's own runtime, not from screen scraping or
transcript-tail heuristics. Screen output cannot distinguish a state change
from a redraw.

**`busy`** — is the engine working. The tmux control-mode output monitor
survives only as the fallback for panes with no native state (terminal panes,
unmanaged CLIs) and as the idle-notify target chooser. That fallback is gated
on the transcript file's mtime advancing in the same window, which is what
suppresses Ink/ratatui frame-redraw spikes being read as work. When the
transcript path cannot be resolved the gate abstains and the monitor stands
alone: idle-notify must not disappear silently for panes the gate cannot
introspect.

**`cliAlive`** — the member's agent runtime is alive, which is not the same as
the pane being alive. Spawned launches do not `exec` over the pane shell, so
the pane survives the CLI, and a retained shell reports `alive` without
`cliAlive`. For codex and grok on a pane the only evidence is a live process
on the pane's TTY: not the pane title, not the `@hive-cli` tag. A pane-less
member has no TTY to read; there the evidence is the daemon's own state for
the threadId or the member key, and its absence reports the runtime dead. For
claude the evidence is the bg job's engine state and not the pane TTY at all:
a viewer gap (reattach window, closed viewer) is not member death.

**`inputState`** — whether the agent is waiting for a human answer. The send
gate consumes it and refuses a send to a waiting target. One waiver exists:
claude parks its status on `waiting` while a `/status`-style dialog is open in
an attached viewer, yet the inbox still queues normally, so that reason alone
does not gate a send.

## Claude: bg job and viewer

A hive claude member is a `claude --bg` job. The engine is a full Claude Code
TUI on a pty owned by claude's own supervisor daemon, running outside tmux;
the member's pane shows it only through an attach viewer. The pane process
table therefore says nothing about the member's life.

Identity is the jobId, which is durable across engine restarts, wakes and
upgrades; the engine pid is not. The sessionId is durable too and stays the
resume/transcript key.

What each signal reports:

- The live engine's session registry entry is the busy/inputState/delivery
  authority. Its `status` vocabulary is observed rather than documented, so an
  unrecognized value must degrade to unknown rather than be trusted. A status
  timestamp older than half an hour demotes the status without touching
  liveness.
- The durable job ledger (`claude agents --json`) costs ~270ms per call and is
  consulted only when the engine entry is missing. Its `state` field lags
  reality and is not used for liveness.
- `jobs/<jobId>/state.json` is deliberately not read: its fields are
  undocumented.

Liveness is three-tier. With no engine entry but a ledger row, the job is
asleep rather than dead: the supervisor parks jobs after about an hour idle. A
wake revives it with the same jobId/sessionId, so an asleep member is not
reaped. A failed ledger read is none of the three tiers: the member keeps
`cliAlive` and reports an unknown input state, because treating an unreadable
ledger as death would reap a live member.

Delivery uses the same wake: when the entry is missing but the ledger still
lists the job, a tty-less attach revives the engine (new pid, same
jobId/sessionId) and delivery re-reads the fresh entry. Only a job missing
from the ledger, or a failed wake, is a delivery error.

Two spawn-time requirements, neither visible at the call site:

- The spawn env is washed of `CLAUDE*`/`ANTHROPIC*` and of the other engines'
  session markers. An inherited `CLAUDE_CODE_CHILD_SESSION` makes the engine
  skip registration entirely, which produces a member that exists and cannot
  be seen; an inherited `CODEX_THREAD_ID` or `GROK_SESSION_ID` keys the
  *spawner's* roster row, so every hive call the new member makes would sign
  as whoever spawned it.
- Path-valued spawn flags must be absolute: they persist verbatim as the job's
  respawn flags.

The pane sits in an attach watch loop because `claude attach` exits 0 both on
user detach and when an engine respawn kicks the viewer; the loop cannot tell
them apart, so it reattaches after a short window the user can break, and only
a non-zero exit (job removed) ends it. `hive kill` parks the job with `claude
stop` rather than destroying it, so the next resume or delivery wakes it. The
hived's supervisor prunes job records whose pane died and parks those orphaned
engines the same way. It does not reattach a viewer: a viewer the user closed
deliberately must not be typed at.

Not every claude member is a bg job. A joined interactive session — a desktop
Claude that ran `hive create` or `hive join` — is a member whose engine is that
session, and its pane is a read-only `hive view` mirror (built at create or
join, or by `hive attach`; `hive mirror` parks or restores it). No CLI
process runs on that pane's tty, so the
pane-keyed probe alone would report the member dead; the roster sessionId is
the engine identity, and while it names a live session that session's registry
status is the member's `cliAlive`, `busy` and `inputState`. `alive` stays the
pane's own fact.

### What the viewer is showing

The attach panel switches sessions in-process, so a member pane can be showing
another member's session, a stranger's, or the panel list, while keeping its
own tags, job record and delivery address. Reading what is on screen is a
separate probe, and each of its steps covers a signal that can be wrong:

- With no viewer process on the pane tty, nothing is displayed: the pane title
  is a latched leftover of what the dead viewer showed last and is not
  evidence.
- Attach-journal entries outlive crashed viewers, so an entry counts only when
  its pid is alive and started when the entry says. No live entry for the
  viewer's pid means the panel list, regardless of the other signals.
- The viewer's argv names the job outright, but only until the process
  re-execs on first entering the panel.
- The pane title carries the viewed session's bare name and is the only
  carrier of which session once the argv is gone. Member jobs are named
  `<team>.<member>`, so a title maps back to a jobId without reading the
  ledger. The match is on token boundaries, so `probe.red2` does not resolve
  to `probe.red`.

This probe resolves display only; nothing on the typing path depends on it.

### Delivery and the receiver's queue

Claude Code wraps every inbox message the model sees in a peer banner and a
security paragraph. The wrapper is hardcoded on the receiving side, keyed to
`origin.kind`, and no field the sender writes can remove it; a pane that shows
the message drawn like typed input is rendering it that way in the display
layer, not receiving a different message. Only the carriage differs. A
`priority: next` frame that lands mid-turn is folded into the running turn at
the next tool boundary, gets no turn of its own, and the model may ignore it.
Everything else (every idle arrival, every `later`) is dequeued into its own
turn and is therefore processed. `now` is not an abort: it lands inside the
running turn, wrapped, and the turn runs on.

Hive's primary lane for a claude member avoids the wrapper: the supervisor
daemon's `op:"reply"` hands the envelope to the worker as its own typed input
on the keystroke lane, `origin:{kind:"human"}`. It lands with no banner in any
state. Idle starts its own turn, which is a mechanical response guarantee;
mid-turn it folds in at the next tool boundary as a bare `❯` line; a blocked
worker takes it on its rv channel. Protocol details live in
[daemon-control-socket.md](daemon-control-socket.md).

When the daemon lane is unavailable the delivery falls back to the inbox
socket with an explicit `priority: next`: a mid-turn arrival folds into the
running turn at the next tool boundary, everything else lands as its own turn
with the peer wrapper. On either lane a folded arrival has no mechanical
guarantee of a response; that obligation is supplied by the member skill's
receipt duty, which teaches the arrival shapes at birth and makes silent skips
a protocol violation. The blind-verified evidence for this split lives in
[reports/wrapped-verdict.html](reports/wrapped-verdict.html). The hived adds
nothing on top: the durable bus row is written, the transport either accepts
or refuses, and scheduling around a mid-turn target belongs to the receiver's
queue, not to hive.

### What a delivery leaves in the receiver's transcript

Between turns, the daemon lane writes only the turn itself: a plain `user` row
with a human origin, carrying the bare envelope, and no queue rows. The inbox
lane between turns is enqueued, dequeued, and lands as a `user` row with a
peer origin and the wrapper.

Mid-turn, both lanes leave an `enqueue`, an `attachment` row of type
`queued_command` carrying the text, and a terminal `queue-operation` `remove`,
and no `user` row for the message at all. The terminal `remove` separates
absorption from delay: a frame that is not `priority: next` (which hive does
not send) is held to the end of the turn and then dequeued into its own
wrapped turn, from the same opening row. The reason string is versioned:
clients from 2.1.246 carry `reason: "absorbed_mid_turn"`, while 2.1.241 and
earlier write the same terminal `remove` with no reason at all. The viewer
(`transcript_view/parser.rs`, per `transcript-view.md`) keys on
`reason == absorbed_mid_turn`, so a transcript written by 2.1.241 or earlier
does not render the absorbed row unless its `queued_command` attachment
carries it.

An absorbed arrival exists only as an attachment and its queue rows, so
nothing downstream (a reader, a viewer, an oracle) can count it as a turn or
read a response obligation out of the file. The receipt duty covers that
obligation; the queue does not.

On the member lane and the `ccd.<name>` lane alike, the frame's `from` is
the message author, never the recipient: `<team>.<sender>` for a member
(`hived/payloads.rs`), a guest's or `ccd.` sender's already-qualified
address verbatim, and the bare team name when hive itself speaks
(`agent/control.rs::origin_label`). That label reaches only the human's
message card; the receiving model sees the text, so the sender also travels
in band, in the `<HIVE from=…>` envelope.

### The member keyboard

Every keyboard path for a claude member (inject, `/compact`, cvim sendback,
interrupt) opens hive's own attach client with stdin on a pipe, writes the
keystrokes, and closes it. The pane's viewer stays attached and unflickered,
and the attach wakes a parked engine, so the park self-heals on the keyboard
path as it does on delivery. Whatever the human has the viewer showing cannot
misroute, block, or be kicked by a delivery. There is no fallback: a member
pane does not get `send-keys`.

Each step of that sequence is there because of a specific failure:

- **Waiting for the client to take the keyboard.** A `C-u` written into a
  client that has not taken it yet is inserted into the composer as a literal
  character instead of clearing it — observed once, with no visible signal
  when it happens.
- **Clearing in a write of its own.** Anything already in the composer would
  otherwise be submitted in front of the delivery.
- **The echo as proof of forwarding.** The engine's own pty output is readable
  headlessly and the composer's unsubmitted content is at the end of it;
  polling until the typed text appears is the only proof the client is
  forwarding stdin. Two details make it evidence rather than coincidence: the
  echo is counted against a snapshot taken before anything was typed, so the
  same text delivered twice does not read as an echo that predates the typing;
  and the on-screen copy may be the head of the text, its tail (the composer
  scrolls to the cursor on a long paste), or a `[Pasted text #N]` placeholder
  holding none of it, so all three shapes count. A slice without an echo
  re-types, and because every attempt re-clears first, a retype cannot double
  the text.
- **Submit verification in the transcript.** A slash command lands as a
  command record; anything else lands as a user turn whose content must equal
  what was typed exactly, and that equality is also the proof that no leftover
  draft rode along. A turn that ends with the text but carries something in
  front of it is reported as a failure, not delivered silently. UI-only slash
  commands write no record at all and degrade to "written".
- **A single Escape.** It leaves no echo, so it skips the echo wait, and a
  second Escape lands on claude's own edit-previous-message chord. An engine
  that was not busy has nothing to interrupt and nothing that could confirm
  one either, so the call returns immediately and reports success rather than
  failing or waiting: cvim sends an Escape before every sendback, and members
  are idle most of the time.

Every subprocess on that path is hard-bounded and its env washed like the
spawn's, and the subcommand must be argv[1]: a leading flag silently
downgrades `attach` into a prompt.

The draft round-trip: the clear drops whatever the human was typing onto
claude's kill ring, and a confirmed submit pastes it back, with the engine
restoring the exact bytes. The paste is gated, because the ring survives a
clear that killed nothing and would otherwise resurrect unrelated content:
only when the member's own pane is certainly-or-likely showing this job does a
styled pane capture (dim-aware, so autocomplete ghost text does not count)
vouch for a real draft. The engine's log replay cannot stand in for that read;
it is an incremental paint stream whose last `❯` can be a history echo rather
than the composer. A re-type forfeits the restore: the second clear overwrites
the single-slot ring with hive's own text. With the gate closed the draft
stays on the ring and the TUI still offers to paste it, so it is recoverable
by hand. The tmux buffer sequence that guards codex and grok drafts does not
apply here: it types at the pane, which is not where a member's keyboard is.

Non-member claude panes (a plain interactive TUI with no job record) are a
separate target rather than a fallback: they keep the tmux keystroke path with
its live-process guard. That guard checks the shape of the claude on the pane
tty, not just its presence, and refuses an attach viewer too, because its
composer belongs to whichever session it is displaying. A member whose job
record went missing therefore fails loudly instead of quietly typing into a
stranger's turn.

An unmanaged claude pane is deliberately unsupported as a member: `hive
create` run from one refuses it and prints the managed-launch fix, and
delivery to a record-less claude pane fails loudly. `hive spawn` does not meet
one, since it launches the engine itself. A claude session that is not on a
pane is the opposite case and is supported: run outside tmux, `hive join`
enrols the calling session with its own sessionId as engine identity, delivery
takes the same two lanes (daemon reply, then the session's own inbox socket),
and its pane is a read-only mirror. Such a member has no bg job, no ledger row,
and none of the keyboard path above applies to it.

## Codex: one shared app-server daemon

One `codex app-server` daemon per `CODEX_HOME` hosts every hive codex thread;
each TUI attaches to its own thread over that socket and hive connects as one
more client, reading state natively from the daemon's status stream instead of
reverse-engineering it from the transcript.

- **Thread identity.** The daemon's env is frozen at spawn time and shared by
  every thread, so identity is the threadId and not the process env; codex
  injects the thread's own id into tool subprocesses instead, and per-pane
  records map threads to panes both ways.
- **Flushing a minted thread.** A minted thread must have its rollout on
  disk before the pane TUI can resume it. The daemon writes the rollout
  lazily (deferred until the first turn), and the TUI resumes in
  paginated-history mode (`thread/resume {excludeTurns}`), which reads the
  source rollout from disk and fails on a thread that has none — while the
  daemon's own bare `thread/resume` on the same loaded thread succeeds. The
  name write is state-DB metadata and never materializes the file; a
  `thread/section/move` to the null section does (the daemon materializes
  and flushes before any placement so placement works ahead of the first
  turn), and leaves only the session header in the file. Hive's contract is
  the file itself: after the flush it checks the path `thread/start`
  reported, and a missing file is a spawn failure. Verified on codex 0.153.2;
  which call materializes has not been stable across codex versions.
- **Trust in remote mode.** It is read from the daemon's config on disk, not
  from the client, so every new cwd gets its trust entry written before its
  thread starts.
- **The daemon is machine-level shared state.** Hive does not kill it: a dead
  daemon takes every attached TUI down with it within seconds. The hived
  supervises instead, respawning while live codex members exist and typing one
  guarded resume into a member's retained shell.
- **State is event-sourced with no time-based staleness gate.** It stays valid
  until the next event. On a shared daemon a client that does not own the turn
  receives only status events, since turn and item events go to the turn's
  owner, so status is the sole busy source; a client that connected late
  backfills once on resume.
- **Active phases are deliberately not subdivided.** The native path trades
  transcript-tail granularity for an authoritative busy edge.
- An unmanaged codex (embedded, or a picker launch whose chosen thread hive
  cannot know) is deliberately unsupported as a member: it still runs, but
  hive reads no state from it and there is no transcript fallback.

## Grok: the leader daemon

A grok member's engine is a `grok agent leader` daemon keyed by identity
(`m-<team>.<member>`), and it is born before any pane exists: spawn raises
the daemon on the member key, asks it for `session/new` with the session id
hive minted, and records that session beside the socket. A tmux pane is a
client attached afterwards — the TUI in it runs `hive grok --resume <sid>`,
resolves the pane's member tags to the same key, and loads the session — the
same engine-first shape as a claude bg job (`claude attach`) and a codex
thread (`codex resume`). Hive attaches as a further ACP client and folds
runtime from that client's notification stream. Only a raw `hive grok` pane
outside any team gets a pane-keyed leader with the pane's lifecycle.

- **Session ownership.** The leader keeps every session of the cwd, so which
  one belongs to this member is not discoverable from it. Hive names the
  session at the mint, records it, and the client ignores notifications for
  any other. A resume keeps the resumed session's own id on the member key; a
  fork has no leader-side primitive, so the pane's TUI branches it under the
  id hive recorded.
- **Session load replay.** Session load replays the session's past updates
  before it answers, so everything received before the load response is
  discarded; a replayed turn must not mark the pane busy. Spawn therefore asks
  the hived to connect once the pane's grok is up on the session, rather than
  lazily on the next tick.
- **Permission requests.** Hive answers its own copy with `cancelled` and
  reports the member as waiting: the decision belongs to the human at the TUI,
  which gets its own copy.
- **No transcript-gate fallback.** That gate knows only the claude and codex
  record shapes and would read a pending grok permission request as clear, so
  a grok pane with no leader state reports unknown instead.
- **Mid-turn prompts.** A prompt sent mid-turn is queued FIFO and runs when
  the turn ends, with no steering and no bounce, the same as typing into the
  TUI. Delivery is therefore accepted at the echo (a queue entry or message
  chunk carrying the text), not at the prompt response, which lands only when
  the whole turn ends.
