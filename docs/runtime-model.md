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
- **Team verbs outside tmux.** create/join/spawn/team/kill/delete/attach and
  view need no tmux context: a team created outside tmux is headless, and a
  spawn with no live display launches the engine alone, addressed directly
  from then on. A pane serves as an address; these verbs do not require one.
  The rest of the surface is display work and refuses to run outside tmux.
- **Engine key scope.** A member's daemon is keyed by `<team>.<member>`, so it
  survives the pane; a raw non-team pane keeps a pane key and pane lifecycle.
  With no pane to ask, an engine still resolves who it is, by a ladder that
  ranks evidence by how hard it is to inherit: the pane's own tags, then the
  roster row the engine keys itself by (a codex thread id, a Claude messaging
  socket), then `HIVE_TEAM`/`HIVE_MEMBER` from the spawn env, then the saved
  context file. Env ranks below the session row because it is inherited, not
  minted: Claude's machine-level bg supervisor daemon freezes the pair of
  whichever member first started it and hands that pair to every engine it
  forks afterwards, so a live team's member can arrive carrying a stranger's
  name. The env rung is roster-verified, and it is the only rung a grok member
  has — its leader daemon keys no session row. The first rung that resolves
  settles the identity, including when it names a different team.
- **Reaping on failed reads.** Daemon reaping does not fire on an unreadable
  registry read, and a young pidfile gets a grace window so a spawn
  mid-registration is not mistaken for an orphan.
- **Team name allocation.** A name-pool pick skips every name the registry
  still lists, so no create lane reuses a name until `hive delete` releases
  it.

`hive render` renders and does not define membership. Only a member with a
recorded engine identity and an attachable cli gets a pane; the rest are named
on stderr and left headless. A claude member whose sessionId names an
interactive session (a joined desktop/ccd session, not a bg job) is rendered
read-only through `hive view`, because the resume lane would mint a forked job
that steals the member's deliveries.

### Mailbox addresses

Of the three send address kinds, only a member names an engine with a
transport. `ccd.<name>` reaches a Claude session outside any team over that
session's own inbox. `flow.run` is the flow runner's mailbox, where delivery
is the durable bus row itself: the runner polls it, owns no transport, and
sends no ack. Mailboxes are listed under `mailboxes`, not in `members`; the
roster holds engines only, and `flow` is a reserved prefix like `ccd`.

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

**`turnPhase`** — the phase of the receiver's turn, per its daemon's events.
Claude emits none: its registry status carries no turn structure and nothing
synthesizes one from the transcript. Consumers must treat an absent
`turnPhase` as "no turn structure available" and fall back to `busy` and the
runtime source, not as an error.

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

- The spawn env is washed of `CLAUDE*`/`ANTHROPIC*`. An inherited
  `CLAUDE_CODE_CHILD_SESSION` makes the engine skip registration entirely,
  which produces a member that exists and cannot be seen.
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
session, and `hive render` gives it a read-only `hive view` mirror pane. No CLI
process runs on that pane's tty, so the pane-keyed probe alone would report the
member dead; the roster sessionId is the engine identity, and while it names a
live session that session's registry status is the member's `cliAlive`, `busy`
and `inputState`. `alive` stays the pane's own fact.

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
wrapped turn, from the same opening row. Consumers must key on the terminal
`remove`, not on its reason string: clients from 2.1.246 carry
`reason: "absorbed_mid_turn"`, while 2.1.241 and earlier write the same
terminal `remove` with no reason at all.

An absorbed arrival exists only as an attachment and its queue rows, so
nothing downstream (a reader, a viewer, an oracle) can count it as a turn or
read a response obligation out of the file. The receipt duty covers that
obligation; the queue does not.

On the member lane, `origin.from` does not name the sender. Hive labels the
inbox frame with the target's own `<team>.<member>`, so a member's transcript
shows its own address on a message someone else sent. The sender travels in
band, in the `<HIVE from=…>` envelope. The `ccd.<name>` lane labels the frame
with the sending member's address instead.

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
and attach renders it read-only. Such a member has no bg job, no ledger row,
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
- **Flushing a minted thread.** A minted thread must be flushed before it is
  resumable: `thread/start` alone leaves the thread unpersisted and a later
  resume fails with no rollout found; the follow-up name write flushes it to
  disk. An unflushed thread is treated as a spawn failure.
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

A born-connected grok pane runs a `grok agent leader` daemon; the TUI attaches
to it and hive attaches as a second ACP client, folding runtime from that
client's notification stream.

- **Session ownership.** The leader keeps every session of the cwd, so which
  one belongs to this pane is not discoverable from it. Hive mints the session
  id at spawn, passes it in, records it, and the client ignores notifications
  for any other.
- **Session load replay.** Session load replays the session's past updates
  before it answers, so everything received before the load response is
  discarded; a replayed turn must not mark the pane busy. Spawn therefore asks
  the hived to connect once the session exists and grok is up, rather than
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
