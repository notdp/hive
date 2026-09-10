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
- **A desktop conversation is followed across its CLI sessions.** A Claude
  session member the desktop app launched enrols with the conversation's
  stable id (`hostSessionId`, from `CLAUDE_CODE_HOST_SESSION_ID`; taken only
  when the session is interactive, desktop-launched, and the desktop's own
  record `claude-code-sessions/<account>/<org>/<id>.json` names it as the
  conversation's current CLI session — a child CLI that merely inherited the
  variable gets none). A rewind-and-resend, a `/clear` or a return to the
  pre-clear session makes the desktop restart the CLI under a new session id
  and move its record's `cliSessionId` on, listing the old id under
  `priorCliSessionIds`; the roster row still names the old id, so the member
  reads as gone. Every 30s the hived (`hived/succession.rs`) moves such a
  row to the record's current session when, and only when, that record
  lists the row's session as a prior, the current one is live, no member
  anywhere holds it, and no other row of any team resolves to it (the tick
  plans over every team's rows and commits its own); the write is a
  compare-and-set under the store lock (`registry::commit_succession`) on
  the observed session *and* host id, so a row rebound or recreated since
  the observation is left alone, and a desktop record the app keeps under
  another account that cannot be listed makes the whole read unknown, and
  `member.session_succeeded` is emitted only for a write that landed
  (`member.session_refused` names the reason otherwise). A conversation the
  human forked has a stable id of its own and never matches — a fork is a
  new session, not the member. A CLI's own rewind keeps its session id and
  needs nothing. Out of scope: a bg job member's `/clear` (its roster
  session id is also its job address; following it needs a job id on the
  row first), and hosts other than the desktop app.
- **The `display` window id.** It is a cache: authority checks do not read
  it, and hived identity is `(workspace socket, team, hive home)`, so a dead
  window does not retire a hived on its own; a missing registry entry with
  no window left behind it does.
- **The hive home is part of the identity.** A hived answers `ping` with
  the `HIVE_HOME` it resolved. A client of the same home that finds another
  build, api version or team on the socket restarts the hived from its own
  binary; a client of another home is refused (`ensure_hived` errors,
  naming both homes) and starts nothing — a replacement would run with the
  client's home, could not see the team's registry, and would reap the
  members it does not own. A hived whose own home holds no registry entry
  for its team exits at start instead of serving.
- **One directory per team.** `$HIVE_HOME/teams/<team>/` holds everything
  hive owns for the team: `team.json` (the entry — present means the team
  exists), and, on the default workspace, `hive.db` (the bus), `run/`
  (`hived.sock`, `notify.jsonl`, `hived.stderr`, `cvim/`) and `artifacts/`.
  `--workspace <DIR>` on `create` puts the workspace elsewhere and the
  entry's `workspace` field records it; `team.json` stays in the team
  directory. `create` always resets the default workspace (a pool name
  recycled after `hive delete` must not inherit the old bus or event log).
  `hive delete` removes `team.json` and leaves the rest; `--down` first
  retires every member and kills the session named after the team only
  when it contains a window marked `@hive-built=1` and `@hive-team=<team>`;
  `--delete-workspace` removes the whole directory (or the external
  workspace); an external workspace is never removed without the flag. A
  long `HIVE_HOME` relocates the hived socket under `/tmp/hive-<uid>/` as
  any long workspace does (`devlog.rs::hived_socket_path_in`).
- **Verbs outside tmux.** The team verbs (create/join/spawn/team/kill/
  delete/attach) need no tmux client: `create` outside tmux puts the
  team window in the session named after the team (created detached when
  missing). Desktop Claude joins with a mirror; a managed terminal session
  transfers its viewer into a pane; raw terminal engines are refused (`join`
  follows the same boundary). `spawn` splits
  a pane into the team's window by id from anywhere, and `attach` rebuilds
  a window that is gone before jumping to it. A pane serves as an address;
  these verbs do not require the caller to have one. `workflow run` rides the
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

The display is eager and never defines membership. Outside-tmux create and
inside-tmux agent create put the team window in the session named after the
team. A shell-pane create still borrows its window. A same-name session is
reusable only when it contains a window marked `@hive-built=1` and
`@hive-team=<team>`; otherwise create or display rebuild reports a conflict.
Pool names skip existing sessions. Every spawn splits into the team window.
`hive attach` reuses an existing display, including a legacy borrowed window,
and adds panes for missing roster members. When the window is gone, attach
rebuilds it in the team session whether the caller is inside or outside tmux.
Only a member with a recorded engine identity and an attachable cli gets a pane; the rest are named on stderr when the window is rebuilt, and
stay registry-only until they have one. A window hive built itself carries
`@hive-built`; `hive delete` closes those except the caller's current window,
and leaves a borrowed window (a shell-pane create or a legacy agent create).
A claude member whose sessionId names an interactive session (a
creating or joined desktop session, not a bg job) is drawn read-only
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
tagged `@hive-hidden <team>`, `@hive-built=1` and `@hive-team=<team>`:
the ownership marks allow a rebuild if only the parked mirror survives.
The viewer keeps running, every team-window scan masks that window (`#{?@hive-hidden,,#{@hive-team}}` — a window format
reads the parked pane's `@hive-team` through), and `hive delete` closes it.
`on` joins the same pane back as the first pane with `join-pane -b`, or
rebuilds it the way a heal would when the parked pane is gone; a heal or
backfill that finds a parked pane of the member joins that one rather than
starting a second viewer. `off` refuses when the mirror is the window's only
pane, because `break-pane` on a lone pane renames the window in place. The
status bar's orch chip and `prefix+m` run the same verb with `--window`: a
`run-shell` job carries no `TMUX_PANE`.

The hived's control client also supplies each pane's OSC 10/11 colour
answers (`tmux/appearance.rs`, tmux 3.5+). Without a report, tmux can answer
black when the control client is the first eligible client and the pane
has a default background. A stored report takes precedence over pane style
while any control client exists on the server; it is not owned by the
client that wrote it.

Reports resolve `HIVE_VIEW_THEME`, then `view.theme`. Auto/system keeps its
existing meaning: an env value of auto bypasses a fixed config preference.
Auto chooses the first non-control client in the same session whose
`client_theme` is dark or light (tmux 3.6+), then falls back to `HIVE_APPEARANCE`,
`COLORFGBG`, and light. A headless session with no explicit preference or
environment hint therefore starts with a provisional light answer.

The monitor samples on attachment and relevant client events. It samples
every two seconds while any non-control client has an empty `client_theme`
or a client event occurred within the last 30 seconds; otherwise it samples
every 60 seconds. Events from other sessions do not extend that fast period;
known clients leaving this session count even if they were not the selected
source. Theme hooks are not control-mode notifications: sampling catches an
initially empty `client_theme` after the terminal reports mode 2031. Each
sample uses one tmux process for `list-clients`; after layout changes the same process
also enumerates panes. One resolved appearance applies to the whole round.
Only a new pane or a different appearance writes new reports. Failed
queries retain the previous snapshot and retry; `pane-colours.selected` in
`run/notify.jsonl` records the source, appearance and selected client.

These reports approximate dark/light with black/white, not the terminal's
exact RGB. A dark terminal attached after hived gets a dark answer once its
997 response has been sampled and the report processed. An application
querying before that point can still read provisional light: there is no
startup barrier and no hot refresh for a running Codex. Updating the cache
does not itself notify applications. Terminals without mode 2031 may leave
`client_theme` empty indefinitely and therefore keep the two-second cadence.
Linked windows share pane overrides across sessions, so sessions with
different themes can overwrite each other's reports; resolving that conflict
is outside this policy. The viewer's
`active_theme_kind` detection chain is unchanged.

The team session hive builds — outside-tmux create, inside-tmux agent create,
or attach rebuilding a lost window from either location — carries Hive's
two-line status bar. It is installed by session id (`tmux/status.rs`):
`status*` and `mouse` are session options, so the source session keeps its
configuration. Borrowed shell-pane and legacy windows get no Hive bar.
The two key bindings below are server-wide, with fallbacks for other windows.
The bar's colours follow the viewer's appearance switch — `view.theme`, `HIVE_VIEW_THEME`, then
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
--window`, and falls through to the saved `MouseDown1Status` binding for
other ranges. With no saved binding it uses tmux's stock `select-window -t =`.
The `prefix+m` binding installed with it is gated on `@hive-team` the same way: its else branch is the command the key ran
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

Every tick that reads the display — the status tick, the claude view tick,
idle-notify and the roster binding they share — runs behind one probe per
tick, `tmux::list_panes_all_status`. While the server answers, that listing
is the pane snapshot those ticks read; while it does not (`no-server` or
`unknown` alike), they are skipped and the probe backs off, doubling from
one tick up to `DISPLAY_PROBE_MAX_BACKOFF_SECONDS`, with
`display.unreachable` / `display.recovered` logged once per flip. The
request socket keeps its one-second accept loop throughout, and the
control-mode monitor's reattach backs off the same way (`tmux/control_mode.rs`,
which also reaps a `tmux -C attach` client of the team session left
reparented to pid 1 by a hived that was killed), so a dead tmux server
costs a hived one probe per 30s instead of a fork storm per second.

### Addresses beyond the roster

Of the send address kinds, only a member names an engine with a transport.
`ccd.<name>` reaches a Claude session outside any team over that session's
own inbox. A `hive workflow run` dispatch has no reply address at all: the
member is never asked to send anything back, and the roster holds engines
only.

### Workflow node: the result is the turn's end, read off the engine

`hive workflow run --team T --name N --cli codex|grok [--model M]` runs one
task on a member the way a Claude Code Workflow runs a subagent: the
member is told nothing about replying, does the task in one turn, and the
result is the last thing it said in that turn. Nothing travels back over
the bus, nothing is read from the engine's transcript, and the member runs
nothing to return: the engine's own turn-end signal is the result's
boundary. A node runs codex or grok — a claude bg job reports no turn end
over any RPC, and Claude Code runs its own subagents natively — and the
runner refuses a claude member before anything is spawned.

- **The dispatch.** The runner mints a dispatch id `nd-<12 lowercase hex>`,
  writes the task to `<workspace>/artifacts/tasks/<name>-<dispatch_id>.md`,
  and hands the hived a `node-dispatch` carrying the id. The hived writes
  the ledger row (`from_agent` empty — there is no sender — `to_agent` the
  member, `artifact` the task path; the only bus write a node makes),
  injects an envelope with no `from` —
  `<HIVE to=<team>.<name> artifact=<that path>>`, body `task <dispatch_id>`
  followed by the task's first line, `</HIVE>` — as **one tracked turn**
  (`Agent::dispatch_turn`): codex `turn/start` on the member's thread,
  whose response carries the turn id; grok `session/prompt` on the
  member's session, whose request id and client generation are kept until
  its response. A Grok result lookup requires the original generation;
  a replacement client cannot supply a result for the old handle. The hived
  holds the engine handle in memory and writes its operation record under
  `run/operations/`, keyed by team incarnation and dispatch id. The record is
  prepared before the bus write or engine submission; the handle and native
  terminal result are then saved with atomic rename (without fsync). Completed
  results remain readable by `node-result` after hived restarts. The run record (below) is written `pending`
  before any of that, so a runner that dies between the delivery and its
  own bookkeeping leaves a pending record behind, never a gap a same-name
  run could walk through.
- **The engine's end of the turn.** The hived's own adapter client is
  the one the engine reports the turn's end to, and it collects the turn's
  text meanwhile. codex: `turn/*` and `item/*` notifications reach only
  the client that started the turn; the client keeps every `agentMessage`
  item's text in item order (`item/completed`; `turn/completed`'s items
  are authoritative when present) and the terminal status —
  `completed`, `interrupted`, `failed` — plus `turn.error`
  (`codex_app_server::TurnResult`). grok (ACP): the `session/prompt`
  response arrives when the turn ends, with `stopReason` (`end_turn`,
  `cancelled`, `max_tokens`, `max_turn_requests`, `refusal`) and
  `_meta.promptId`; the text comes from the `session/update`
  `agent_message_chunk`s whose `_meta.promptId` matches, split into
  segments at each `tool_call`, the last non-empty segment being the
  result — a turn that says something, runs a tool, then answers, returns
  the answer (`grok_leader::PromptResult`). In both engines the result is
  the member's last message of the turn; a member that stops to ask has
  ended its turn with that question.
- **Retirement.** Shutdown, identity replacement and reexec close the same
  admission gate and wait for accepted request leases and outstanding native
  results to be persisted. Ordinary Codex/Grok sends also retain tracked
  handles until their terminal results are saved. Claude sends record the
  transport acceptance only: its inbox/job is external to the hived and no
  native execution result is available. Unresolved live handles or failed
  journal writes defer voluntary retirement and emit a diagnostic. New requests
  during shutdown drain are rejected as `notAdmitted`, so callers can retry
  without replaying accepted work. Explicit member/team removal is
  recorded as interruption; an abrupt daemon exit leaves an ambiguous record.
- **The read-back.** The runner polls the hived's `node-result` for the
  dispatch id at 1s: `running` while the turn is open; `ended` with
  `status` (the engine's word), `text` and `error` once it is; `unknown`
  with a `reason` when no operation is recorded for the id. A journaled
  operation whose outcome cannot be recovered returns `ambiguous`, including
  a restart before its terminal result was saved, a missing turn id, or loss
  of the original adapter client. The runner immediately retains an `unknown`
  record for `ambiguous`; it does not classify that as `no_result` or resend. `unknown` is never a verdict on the turn: with the member's
  turn open or unanswered the runner keeps waiting (the turn may still end
  in front of a client that never saw it start), and only 5 consecutive
  unknowns with the turn closed (`turn-open` `false`) end the run
  `no_result`. No answer at all from the hived on 120 consecutive polls
  returns `unknown`: the waiter exits, but the record retains ownership
  because execution has not been resolved. A member the roster reports
  dead is `member_gone`. The turn itself has no timeout (the caller
  decides how long to wait).
- **A refused dispatch and a lost answer are different failures.** The
  hived's answer to the dispatch is what the runner keys on
  (`send.rs::DispatchFailure`). `Refused` is a definite no — the hived
  answered `ok:false` (transport refused, unknown member, send gate) or
  the request never reached it (no socket, connect or write failed): the
  task is not with the member, the dispatch is retried up to three times,
  and a final refusal takes the pending record back, retires a member
  this run spawned, and exits 1. `Unknown` is the request going out whole
  and no usable answer coming back (read timeout, dropped connection,
  empty or unparsable reply): the hived may have injected the task, so it
  is never sent again. The run keeps its pending record (`seq` stays
  null, since the seq rode the lost answer) and reads the turn back
  exactly as a delivered dispatch — the hived holds the turn under the
  dispatch id whether or not its answer arrived, so a lost answer costs
  nothing but the seq when the hived still holds a usable turn handle.
  The engine boundary follows the same rule: Codex requests not written
  or explicitly rejected are retryable; write failures and lost responses
  are `Unknown`. The hived retains an unknown handle and returns
  `dispatchUnknown`, which the runner treats as a lost answer, not a
  refusal. It cannot recover a turn id from that response, so result
  queries remain unknown.
- **Readiness.** The runner dispatches only between turns, and only on a
  positive reading from the engine's own daemon that no turn is open. The
  runner asks the hived's `turn-open` for the member and the hived queries
  the engine directly, with no tick cache in between (codex: the
  app-server's `thread/read`; grok: the leader pool's push-fed turn
  evidence, the session load replay included). No answer says nothing
  about the turn and never opens the dispatch; every null answer carries
  a `reason` and is recorded as a `turn_open.null` notify event (cli,
  agent, reason), so a run stalled on null leaves evidence in
  `notify.jsonl`. A member still in a turn after 600 polls ends the run
  `member_busy` without dispatching — a task dropped into a running turn
  would be folded into it.
- **The JSON line** (stdout, exit 0 whenever a verdict was reached):
  `status`, `name`, `pane` (may be empty), `reused`, `dispatchId`, `body`
  (the member's last message of the turn, possibly empty) whenever the
  turn ended, `reason` for every status but `completed`. `status` is one
  of `completed` (codex `completed`, grok `end_turn`) | `interrupted`
  (codex `interrupted`, grok `cancelled`; `body` is what was said by
  then) | `failed` (any other engine word — codex `failed`, grok an error
  response, `max_tokens`, `refusal`…; `reason` carries the word and the
  engine's error) | `no_result` | `unknown` (waiter lost contact; execution
  unresolved) | `member_gone` | `member_busy` (a pending or unknown
  node record for the member whose member is alive, the per-member lock
  held by another runner, or the readiness cap above). No session, turn
  or artifact field: the runner never learns the engine's session, and a
  member that wants to hand over a file names its path in what it says.
  stderr and exit 1 mean the task was not dispatched — bad team, a claude
  member, spawn or ready failure, the dispatch refused — and the run can
  be repeated (`member_busy` is the other not-dispatched verdict, reported
  as a JSON line because it names a state the caller acts on); a
  dispatched task always ends in a JSON line.
- **The record.** `<workspace>/run/workflow/<name>.json` — `dispatchId`,
  `cli`, `status` (`pending | unknown | <terminal status>`), `body`/`reason` from
  the waiter's verdict, `seq` (ledger seq of the dispatch, filled in after the
  delivery), `startedAt` (epoch seconds) — under the flock
  `<workspace>/run/workflow/<name>.lock` held for the whole run; the lock
  file itself is never deleted, the record is. A stale pending record whose
  member is dead is replaced by the next run; `hive kill` of the member
  removes its record. A same-name node reuses a live member, whatever
  `--cli` says (its engine is the roster's). After acquiring the lock, a
  new run checks a live member's pending record, including status
  `unknown` from 120 unanswered polls, once by its old dispatch id.
  `Ended` saves the old result before continuing; `Unknown` with an
  explicitly closed turn settles the old record as `no_result` and
  continues. `Running`, no result answer, or `Unknown` with an open or
  unanswered turn returns `member_busy` with the old dispatch id.
  The new task still passes the normal turn-closed gate before dispatch.
  The per-member record is replaced when the new task starts; this is
  reconciliation on reuse, not a result archive or a resume command.

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
  as whoever spawned it. Cold spawn and wake use the pane's terminal:
  `TERM` is tmux's `default-terminal` (fallback `tmux-256color`),
  `COLORTERM=truecolor`, and inherited `NO_COLOR` is removed.
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

On the inbox lane hive writes the envelope inside Claude Code's own
peer-message tag, `<cross-session-message from="<sender>">` … `</cross-session-message>`
(`claude_sessions::peer_card_envelope`), byte-for-byte the shape the
receiver's `SendMessage` builds. The receiver's message card — the terminal's
`UserCrossSessionMessage`, the desktop's peer card — parses the row's text for
that shape and, when it parses, draws `@ <sender>` over the inner envelope
alone; a bare `<HIVE>` body does not parse and is drawn whole, lead line and
safety paragraph included (which is why a hive message used to show its
wrapper on screen while a native one did not). This changes what a human
sees and nothing else: the row still stores the wrapper, the model still
reads it, and the frame's `from` (the origin) still names the sender, the
same name the tag carries (the desktop card checks the two agree for a
`local_…` origin). The transcript viewer peels the tag the same way it
peels the wrapper.

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

When hive enrols a claude TUI from a tmux pane, that pane must have a hive
background-job binding. Both `hive create` and `hive join` enforce this on the
target pane before anything is written (`team::claude_pane_job_gate`);
`--no-notify` skips only the join message and its reachability check. A bare
interactive claude TUI is not supported as a pane member: it can receive over
its own inbox, but it has none of the keyboard lane above and no park/wake
lifecycle, and a pane's human can choose the managed launcher instead.
Delivery to a claude pane with neither a job binding nor a deliverable
session id fails loudly. `hive spawn` and `hive fork` are not gated, since
they launch the engine themselves and the binding lands when it starts.

The desktop app's interactive claude session, joining from outside tmux, is
enrolled by its session id instead (its registry entry's `entrypoint` is
`claude-desktop`; a terminal's `cli` session, or one with no entrypoint, is
refused by create and join and pointed at `hclaude`). Hive gives it a
read-only mirror pane; that display does not turn it into a background-job
member. It receives session messages over the same two lanes (daemon reply,
then the session's own inbox socket), has no bg job and no ledger row, and
none of the keyboard path above applies to it. This is an enrolment policy,
not a limitation of the session's inbox transport: a terminal's claude still
reaches a team as a `ccd.<name>` guest over the same socket.

## Terminal launchers and team handoff

Inside tmux, managed agent create moves the existing viewer pane into the
team session with checked `swap-pane`. The pane id and engine binding stay
the same. A placeholder running `sleep` takes its old slot. After the
registry commit, it is replaced with a shell when the source window had
one pane, or removed when other panes remain. Interactive shell startup
is deferred until that commit.
A source window linked across sessions is refused before moving anything.
Failures before the registry commit swap the viewer back and remove the
placeholder; an unconfirmed pane location or failed swap back leaves both
panes for recovery. Once registered, errors are reported without undoing the
team. New team sessions receive the engine roots listed below.

Before moving, create records the non-control clients displaying the source
window. With exactly one, it switches that client explicitly to the team
window. With none or more than one, it leaves clients in place and adds a
`hive attach <team>` hint to the create result. A failed switch also leaves
the team registered with the hint. The normal terminal handoff protocol
below is used only outside tmux; moving a pane does not restart its viewer.

Outside tmux, `hive claude`, `hive codex`, and `hive grok` show a local
viewer without creating a tmux session (`cli/launch.rs`,
`terminal_handoff/`). Their engines are a Claude background job, a thread
on the shared Codex app-server, and a Grok launch leader. Each launcher
holds a per-session lock and publishes a private control socket under its
engine config tree's `hive-control/`. The launch token authenticates
create/join requests; a bare engine has no launcher to transfer its viewer.

Create/join prepares a team pane, then asks the launcher to release its
viewer. The launcher terminates its foreground process group, including
an npm wrapper's native child, and reaps its owned child before the CLI
writes the native binding and commits the roster row. The row is the
boundary: a disconnected request before that write removes the prepared
pane and restores a viewer on the same session; after that write it starts
the team viewer and keeps the membership. Rollback resumes without
replaying the initial prompt. The launcher restores the terminal and runs
a tmux client. A viewer command being scheduled is not proof it painted a
frame; later viewer or client-attach failures leave the enrolled session
recoverable through the team's display.

A new team session gets the team status bar and the launcher's roots
(`HIVE_HOME`, `CLAUDE_HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GROK_HOME`).
Set, empty and unset values survive an existing server's globals; unset
values use `set-environment -r`. The viewer command carries those roots and
PATH across a login shell. Join leaves an existing session's environment
alone. Native bindings connect the same engine ID to its member: Claude's
pane-job record, Codex's pane-thread record, or Grok's member alias to its
launch key. The engine's process environment does not change.

Detaching returns to the shell. Resuming an enrolled session outside tmux
opens its team display. Kill/delete keep each engine's member lifecycle;
they do not stop the shared Codex daemon. Grok stops an unbound launch
leader when its local launcher exits. Moving a session back to a standalone
viewer on team teardown is not implemented. Management commands,
non-interactive launches, explicit remote endpoints and pickers whose
session ID is not known before launch retain their native behavior.

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
- **The daemon is machine-level shared state.** Hive does not kill it for
  pane or team lifecycle: a dead daemon takes every attached TUI down with
  it within seconds. The hived supervises instead, respawning while live
  codex members exist and typing one guarded resume into a member's
  retained shell.
- **Auth is loaded once and only reloaded for the same account.** For
  managed ChatGPT auth, codex's auth manager reloads `auth.json` only when
  the on-disk account id equals the cached one
  (`reload_if_account_id_matches`, on the pre-refresh reload and on the
  401 recovery; verified on codex 0.153.4). A login to another account or
  workspace, or a login while the daemon holds no account, leaves the
  daemon unable to recover once its token needs a refresh or is refused:
  every turn ends with "Your access token could not be refreshed because
  you have since logged out or signed in to another account", and no RPC
  reloads unconditionally. Hive records the account id the daemon was
  spawned with beside the pidfile (`hive-shared.auth`, `auth_guard.rs`,
  read from the disk before the child starts); the supervisor tick and
  `spawn_daemon` compare it with the disk's `tokens.account_id` (never a
  token) and replace the daemon on a change (`codex.daemon.auth_stale`,
  then the ordinary `codex.daemon.respawn`). A daemon without a baseline
  is asked over `account/rateLimits/read`, whose answer names the account
  of the token the daemon holds (`codex.daemon.auth_settle`); an
  unreadable `auth.json` (a login mid-write) is never a change. The
  replacement runs under one flock per CODEX_HOME (`hive-shared.lock`)
  across every process that may do it, and every baseline write is inside
  that same critical section — the hived's tick reads the verdict
  lock-free and writes nothing, so a daemon's answer cannot land over a
  replacement another process just committed. The hived drops its own
  daemon client only when a daemon was actually started: the client of a
  reused daemon holds the tracked turns whose results a workflow runner
  still reads back (`node-result`). The recorded pid is
  signalled only while it is still this socket's `codex app-server`, and
  the records are cleared only once the process is gone (codex stops
  listening before it finishes shutting down, so a silent socket is not a
  gone daemon). Attached TUIs reconnect on their own.
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
A terminal launcher uses `l-<id>` instead. On create/join, the member's
`m-<team>.<member>.alias` names that launch key; socket and session-record
lookups follow it, so binding does not rename a live socket or restart the
leader. The alias also lets member teardown reach that leader.

- **Session ownership.** The leader keeps every session of the cwd, so which
  one belongs to this member is not discoverable from it. Hive names the
  session at the mint, records it, and the client ignores notifications for
  any other. A resume keeps the resumed session's own id on the member key; a
  fork has no leader-side primitive, so the pane's TUI branches it under the
  id hive recorded.
- **Session load replay.** Session load replays the session's past updates
  before it answers. For the display that replay is discarded: a replayed
  turn must not mark the pane busy, so spawn asks the hived to connect once
  the pane's grok is up on the session, rather than lazily on the next tick.
  For the dispatch gate it is evidence: the replay is the engine's own turn
  history, and its last turn event (a message chunk or tool call opens,
  `turn_completed` closes) is the session's state at load time — so a
  hived restarted onto an idle member answers `turn-open` `false` at once
  instead of `null` until the member's next turn.
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
