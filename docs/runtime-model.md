# Hive Runtime Model

Why the runtime is shaped the way it is: where each fact about a running
member comes from, which layer owns which truth, and what the delivery
transports do with a message once it reaches an engine.

Boundary with [transcript-view.md](transcript-view.md): this document owns
what is true of a **running member** — the state its engine reports and what
its transport does with a message. That one owns what a **read-only observer**
can recover from a session's JSONL, which is strictly less. The transcript
holds only what was written to it, and a delivery folded into a running turn
writes no turn of its own.

## Out of scope by decision

Hive does not define a semantic global `busy`/`idle` truth, automatic
scheduling, automatic fork/spawn decisions, or automatic garbage collection.
It reports what each engine reports about itself and refuses to synthesize a
scheduler on top. Scheduling around a busy target is the receiver's queue's
job.

## Registry is truth, tmux is display

A team exists because it has a **registry entry**, not because a window
renders it. tmux is a display layer resolved on top: window options and pane
tags say *where* the team is rendered, never *whether* it exists. Deleting
the window is closing a screen; `hive delete` is the team's end of life.

Consequences that are load-bearing across modules:

- **Membership has one writer.** The CLI adds and removes roster names;
  the hived only backfills fields of names already there. An observation
  racing a `hive kill` must never resurrect the killed member.
- **The `display` window id is a cache.** Authority checks never read it, and
  hived identity is `(workspace socket, team)` — a dead window no longer
  retires a hived on its own; a missing registry entry with no window left
  behind it does.
- **The team verbs work anywhere.** create/join/spawn/team/kill/delete/attach
  and view need no tmux context: a team created outside tmux is headless, and a
  spawn with no live display launches the engine alone, addressed directly from
  then on. The pane is an address, never a prerequisite. The rest of the surface
  is display work and refuses to run outside tmux.
- **Engine keys follow the member, not the pane.** A member's daemon is keyed
  by `<team>.<member>`, so it survives the pane; a raw non-team pane keeps a
  pane key and pane lifecycle. Engines also carry their member identity in
  env, so a tool subprocess can resolve who it is with no pane at all — the
  fallback behind a live pane binding.
- **Absence of evidence is not evidence of death.** Daemon reaping never
  fires on an unreadable registry read, and a young pidfile gets a grace
  window so a spawn mid-registration is not mistaken for an orphan.
- **Team names are owned by the registry.** A name-pool pick skips every name
  the registry still lists, so no create lane reuses a name until
  `hive delete` releases it.

`hive attach` renders; it never defines. Only a member with a recorded engine
identity and an attachable cli gets a pane; the rest are named on stderr and
left headless. A claude member whose sessionId names an **interactive**
session (a joined desktop/ccd session, not a bg job) is rendered read-only
through `hive view`, because the resume lane would mint a forked job that
steals the member's deliveries.

### Mailboxes are not engines

Of the three send address kinds only a member names an engine with a
transport. `ccd.<name>` reaches a Claude session outside any team over that
session's own inbox; `flow.run` is the flow runner's mailbox, where delivery
is the durable bus row itself — the runner polls it, owns no transport, and
never acks. Mailboxes are listed under `mailboxes`, never in `members`: the
roster stays engines-only, and `flow` is a reserved prefix the way `ccd` is.

## Runtime fields: meaning, and where truth comes from

Every field comes from the CLI's **own** runtime — never screen scraping,
never transcript-tail heuristics. A native source has an authoritative edge; a
screen has redraws.

**`busy`** — is the engine working. The tmux control-mode output monitor
survives only as the fallback for panes with no native state (terminal panes,
unmanaged CLIs) and as the idle-notify target chooser. That fallback is gated
on the transcript file's mtime advancing in the same window, which is what
suppresses Ink/ratatui frame-redraw spikes being read as work. When the
transcript path cannot be resolved the gate abstains and the monitor stands
alone: idle-notify must never silently disappear for panes the gate cannot
introspect.

**`cliAlive`** — the member's agent runtime is alive, which is not the same as
the pane being alive. Spawned launches do not `exec` over the pane shell, so
the pane survives the CLI, and a retained shell reports `alive` without
`cliAlive`. For codex and grok on a pane the only evidence is a live process
on the pane's TTY — never the pane title, never the `@hive-cli` tag. A
pane-less member has no TTY to read: there the daemon's own state for the
threadId or the member key is the evidence, and its absence is what reports
the runtime dead. For claude it is the bg job's engine state and never the
pane TTY at all: a viewer gap (reattach window, closed viewer) is not member
death.

**`inputState`** — whether the agent is waiting for a human answer. Its
important consumer is the send gate, which refuses a send to a waiting target.
One waiver exists: claude parks its status on `waiting` while a `/status`-style
dialog is open in an attached viewer, yet the inbox still queues normally, so
that reason alone does not gate a send.

**`turnPhase`** — the phase of the receiver's turn, per its daemon's events.
Claude emits none: its registry status carries no turn structure and nothing
synthesizes one from the transcript. Consumers must treat an absent
`turnPhase` as "no turn structure available" and fall back to `busy` and the
runtime source, not as an error.

## Claude: the job is the member, the pane is a viewer

A hive claude member is a **`claude --bg` job**. The engine is a full Claude
Code TUI on a pty owned by claude's own supervisor daemon, running outside
tmux; the member's pane only shows it through an attach viewer. The pane
process table therefore says nothing about the member's life: the viewer is
furniture, the job is the member.

Identity is the **jobId** — durable across engine restarts, wakes and
upgrades, which the engine pid is not. The sessionId is durable too and stays
the resume/transcript key.

What each signal is worth:

- The live engine's session registry entry is the busy/inputState/delivery
  authority. Its `status` vocabulary is *observed, not documented*, so an
  unrecognized value must degrade to unknown rather than be trusted. A status
  timestamp older than half an hour demotes the status without touching
  liveness — a quiet engine is not a dead one.
- The durable job ledger (`claude agents --json`) costs ~270ms per call and is
  consulted only when the engine entry is missing. Its `state` field lags
  reality and is never used for liveness.
- `jobs/<jobId>/state.json` is deliberately **not** read: undocumented fields.

Liveness is three-tier, and the middle tier is the point: with no engine entry
but a ledger row, the job is **asleep** — the supervisor parks jobs after
about an hour idle — not dead. A wake revives it with the same
jobId/sessionId, so an asleep member is never reaped. A *failed* ledger read
is none of the three tiers: the member keeps `cliAlive` and reports an unknown
input state. An unreadable ledger is not evidence of death, and treating it as
one would reap a live member.

Delivery self-heals through the same wake primitive: when the entry is missing
but the ledger still lists the job, a tty-less attach revives the engine — new
pid, same jobId/sessionId — and delivery re-reads the fresh entry. Only a job
missing from the ledger, or a failed wake, is a delivery error.

Spawn-time traps, both invisible at the call site:

- The spawn env is washed of `CLAUDE*`/`ANTHROPIC*`. An inherited
  `CLAUDE_CODE_CHILD_SESSION` makes the engine skip registration entirely,
  which produces a member that exists and can never be seen.
- Path-valued spawn flags must be absolute: they persist verbatim as the job's
  respawn flags.

The pane sits in an attach watch loop because `claude attach` exits 0 both on
user detach and when an engine respawn kicks the viewer — the loop cannot tell
them apart, so it reattaches after a short window the user can break, and only
a non-zero exit (job removed) ends it. `hive kill` parks the job with `claude
stop` rather than destroying it, so the next resume or delivery wakes it. The
hived's supervisor prunes job records whose pane died and parks those orphaned
engines the same way; it never reattaches a viewer, because a user who closed
one deliberately must not be typed at.

### The viewer can be showing anything

The attach panel switches sessions in-process, so a member pane can be showing
another member's session, a stranger's, or the panel list, while keeping its
own tags, job record and delivery address. Reading *what is on screen* is a
separate probe, and each of its steps exists because a simpler signal lies:

- With no viewer process on the pane tty, nothing is displayed — the pane
  title is a latched leftover of what the dead viewer showed last, never
  evidence.
- Attach-journal entries outlive crashed viewers, so an entry counts only when
  its pid is alive and started when the entry says. No live entry for the
  viewer's pid means the panel list, whatever the other signals say.
- The viewer's argv names the job outright, but only until the process
  re-execs on first entering the panel.
- The pane title carries the viewed session's bare name and is the only
  carrier of *which* once the argv is gone. Member jobs are named
  `<team>.<member>`, so a title maps back to a jobId without paying for the
  ledger — matched on token boundaries, so `probe.red2` never resolves to
  `probe.red`.

This is **display truth only**. Nothing about typing depends on it.

### Delivery rides the receiver's own queue

Claude Code wraps every inbox message the model sees in a peer banner and a
security paragraph. The wrapper is hardcoded on the receiving side, keyed to
`origin.kind`, and no field the sender writes can remove it — a pane that
shows the message drawn like typed input is a display-layer rendering, not a
different message. What varies is carriage: a `priority: next` frame that
lands mid-turn is folded into the running turn at the next tool boundary,
where it never gets a turn of its own and the model is free to ignore it;
everything else — every idle arrival, every `later` — is dequeued into its own
turn, which is what guarantees it gets processed. `now` is not an abort: it
lands inside the running turn, wrapped, and the turn runs on.

Hive's primary lane for a claude member sidesteps the wrapper entirely: the
supervisor daemon's `op:"reply"` hands the envelope to the worker as its own
typed input — `origin:{kind:"human"}`, the keystroke lane — so it lands with
no banner in any state: idle starts its own turn (a mechanical response
guarantee), mid-turn folds in at the next tool boundary as a bare `❯` line,
and a blocked worker takes it on its rv channel. Protocol details live in
[daemon-control-socket.md](daemon-control-socket.md).

When the daemon lane is unavailable the delivery falls back to the inbox
socket with an explicit `priority: next`: a mid-turn arrival folds into the
running turn at the next tool boundary, everything else lands as its own turn
wearing the peer wrapper. On either lane a folded arrival has no mechanical
guarantee of a response; that obligation is supplied by the member skill's
receipt duty, which teaches the arrival shapes at birth and makes silent skips
a protocol violation. The blind-verified evidence for this split lives in
[reports/wrapped-verdict.html](reports/wrapped-verdict.html). The hived adds
nothing on top: the durable bus row is written, the transport either accepts
or refuses, and scheduling around a mid-turn target is the receiver's queue's
job, not hive's.

### What a delivery leaves in the receiver's transcript

Between turns, the daemon lane writes nothing but the turn itself — a plain
`user` row with a human origin, carrying the bare envelope, no queue rows at
all. The inbox lane between turns is enqueued, dequeued, and lands as a `user`
row with a peer origin wearing the wrapper.

Mid-turn, both lanes leave an `enqueue`, an `attachment` row of type
`queued_command` carrying the text, and a terminal `queue-operation` `remove` —
and no `user` row for the message at all. That terminal `remove` is what
separates absorption from mere delay: a frame that is *not* `priority: next`
(which hive never sends) is held to the end of the turn and then dequeued into
its own wrapped turn, from the same opening row. **Key on the terminal
`remove`, never on its reason string**: clients from 2.1.246 carry
`reason: "absorbed_mid_turn"`, while 2.1.241 and earlier write the same
terminal `remove` with no reason at all.

The cost of folding is that an absorbed arrival exists only as an attachment
and its queue rows, so nothing downstream — a reader, a viewer, an oracle —
can count it as a turn or read a response obligation out of the file. The
receipt duty covers it; the queue does not.

One receiving-side trap: on the member lane, `origin.from` does **not** name
the sender. Hive labels the inbox frame with the *target's* own
`<team>.<member>`, so a member's transcript shows its own address on a message
someone else sent. The real sender travels in band, in the `<HIVE from=…>`
envelope. The `ccd.<name>` lane is the exception: it labels the frame with the
sending member's address.

### The member keyboard is the job, not the pane

Every keyboard path for a claude member — inject, `/compact`, cvim sendback,
interrupt — opens hive's own attach client with stdin on a pipe, writes the
keystrokes, and closes it. The pane's viewer stays attached and unflickered,
and the attach wakes a parked engine, so the park self-heals on the keyboard
path exactly as it does on delivery. The pane's viewer is a screen: what the
human has it showing can no longer misroute, block, or get kicked by a
delivery. There is no fallback — a member pane never gets `send-keys`.

Each step of that sequence is there because of a specific failure:

- **Wait for the client to take the keyboard.** A `C-u` written into a client
  that has not taken it yet is inserted into the composer as a literal
  character instead of clearing it — observed once, and silent when it
  happens.
- **Clear in a write of its own.** Anything already in the composer would
  otherwise be submitted in front of the delivery.
- **Treat the echo as the proof, not a sleep.** The engine's own pty output is
  readable headlessly and the composer's unsubmitted content is at the end of
  it; polling until the typed text appears is the only proof the client is
  forwarding stdin. Two details make it evidence rather than coincidence: the
  echo is counted against a snapshot taken before anything was typed, so the
  same text delivered twice does not read as an echo that predates the typing;
  and the on-screen copy may be the head of the text, its tail (the composer
  scrolls to the cursor on a long paste), or a `[Pasted text #N]` placeholder
  holding none of it, so all three shapes count. A slice without an echo
  re-types, and because every attempt re-clears first, a retype cannot double
  the text.
- **Verify the submit in the transcript.** A slash command lands as a command
  record; anything else lands as a user turn whose content must equal what was
  typed **exactly** — equality is also the proof that no leftover draft rode
  along. A turn that ends with the text but carries something in front of it
  is reported as a failure, not delivered silently. UI-only slash commands
  write no record at all and degrade to "written".
- **Escape exactly once.** It leaves no echo, so it skips the echo wait, and a
  second Escape lands on claude's own "edit previous message" chord. An engine
  that was not busy has nothing to interrupt and nothing that could confirm
  one either, so that returns immediately: a success, not a failure and not a
  wait — cvim sends an Escape before every sendback, and members are idle most
  of the time.

Every subprocess on that path is hard-bounded and its env washed like the
spawn's, and the subcommand must be argv[1]: a leading flag silently downgrades
`attach` into a prompt.

The draft round-trip: the clear drops whatever the human was typing onto
claude's kill ring, and a confirmed submit pastes it back — the engine itself
restores the exact bytes. The paste is gated, because the ring survives a
clear that killed nothing and would otherwise resurrect unrelated content:
only when the member's own pane is certainly-or-likely showing this job does a
styled pane capture (dim-aware, so autocomplete ghost text never counts) vouch
for a real draft. The engine's log replay cannot stand in for that read — it is
an incremental paint stream whose last `❯` can be a history echo, not the
composer. A re-type forfeits the restore: the second clear overwrites the
single-slot ring with hive's own text. With the gate closed the draft stays on
the ring and the TUI still offers to paste it — recoverable by hand, never
silently lost. The tmux buffer dance that guards codex and grok drafts does not
apply here: it types at the pane, and the pane is not where a member's keyboard
is.

Non-member claude panes — a plain interactive TUI with no job record — are a
different target, not a fallback: they keep the tmux keystroke path with its
live-process guard. That guard checks the *shape* of the claude on the pane
tty, not just its presence: an attach viewer is refused too, because its
composer belongs to whichever session it is displaying. So a member whose job
record went missing fails loudly instead of quietly typing into a stranger's
turn.

An unmanaged claude **pane** is deliberately unsupported as a member: `hive
create` run from one refuses it and prints the managed-launch fix, and
delivery to a record-less claude pane fails loudly. `hive spawn` never meets
one — it launches the engine itself. A claude session that is **not on a pane**
is the opposite case and is supported: run outside tmux, `hive join` enrols the
calling session with its own sessionId as engine identity, delivery takes the
same two lanes (daemon reply, then the session's own inbox socket), and attach
renders it read-only. Such a member has no bg job, no ledger row, and none of
the keyboard path above applies to it.

## Codex: one shared app-server daemon

One `codex app-server` daemon per `CODEX_HOME` hosts every hive codex thread;
each TUI attaches to its own thread over that socket and hive connects as one
more client, reading state natively from the daemon's status stream instead of
reverse-engineering it from the transcript.

- **Identity is the threadId, never the process env.** The daemon's env is
  frozen at spawn time and shared by every thread; codex injects the thread's
  own id into tool subprocesses instead, and per-pane records map threads to
  panes both ways.
- **A minted thread must be flushed before it is resumable.** `thread/start`
  alone leaves the thread unpersisted and a later resume fails with no rollout
  found; the follow-up name write flushes it to disk. An unflushed thread is
  treated as a spawn failure.
- **Trust in remote mode is read from the daemon's config on disk**, not from
  the client, so every new cwd gets its trust entry written before its thread
  starts.
- **The daemon is machine-level shared state.** Hive never kills it — a dead
  daemon takes every attached TUI down with it within seconds. The hived
  supervises instead: respawn while live codex members exist, and one guarded
  resume typed into a member's retained shell.
- **State is event-sourced and has no time-based staleness gate**; it stays
  valid until the next event. On a shared daemon a client that does not own
  the turn receives only status events — turn and item events go to the turn's
  owner — so status is the sole busy source, and a client that connected late
  backfills once on resume.
- **Active phases are deliberately not subdivided**: the native path trades
  transcript-tail granularity for an authoritative busy edge.
- An unmanaged codex — embedded, or a picker launch whose chosen thread hive
  cannot know — is deliberately unsupported **as a member**: it still runs, but
  hive reads no state from it and there is no transcript fallback.

## Grok: the leader daemon

A born-connected grok pane runs a `grok agent leader` daemon; the TUI attaches
to it and hive attaches as a second ACP client, folding runtime from that
client's notification stream.

- **The leader keeps every session of the cwd**, so which one belongs to this
  pane is not discoverable from it. Hive mints the session id at spawn, passes
  it in, records it, and the client ignores notifications for any other.
- **Session load replays the session's past updates before it answers**, so
  everything received before the load response is discarded — a replayed turn
  must never mark the pane busy. This is why spawn asks the hived to connect
  once the session exists and grok is up, rather than lazily on the next tick.
- **Hive answers its own copy of a permission request with `cancelled`** and
  reports the member as waiting: the decision belongs to the human at the TUI,
  which gets its own copy.
- **Grok never falls back to the transcript gate.** That gate knows only the
  claude and codex record shapes and would read a pending grok permission
  request as clear, so a grok pane with no leader state reports unknown
  instead.
- **A prompt sent mid-turn is queued FIFO and runs when the turn ends** — no
  steering, no bounce, the same as typing into the TUI. Delivery is therefore
  accepted at the *echo* (a queue entry or message chunk carrying the text),
  not at the prompt response, which only lands when the whole turn ends.
