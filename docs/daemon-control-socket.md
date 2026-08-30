# The claude supervisor daemon's control socket

The Claude Code bg supervisor's control protocol, recovered from the shipped
binary and probed live. This is an unpublished surface: every claim here is
versioned, and the daemon's own `EAUTH` text tells clients to "stop driving
the control socket directly". Hive drives exactly one verb, `reply`, the
claude-member delivery lane (`claude_sessions::daemon_reply`). The rest is
recorded because several verbs look like replacements for machinery hive
hand-rolls, and because what they actually do rules most of them out.

## The pin

Pinned to **2.1.240**, still the installed version, and the running supervisor
agrees: `{"proto":1,"op":"ping"}` answers `{"version":"2.1.240","proto":1}`.
The surveyed bundle is `~/.local/share/claude/versions/2.1.240` (Mach-O,
bun-compiled, JS bundle embedded as plain text), `BUILD_TIME
2026-08-22T05:07:39Z`, `GIT_SHA d235569e3f61fc4d9aacd7c85e6d1b6253e03f52`.

Every claim below must be re-verified on upgrade. To re-recover, find the
request handler by a stable literal rather than a minified name, since the
names change every build:

```
strings -n 3 <binary> | grep -n "job isn't accepting replies"
```

The hit lands inside the control server's `switch(op)`; the zod request
schema (a `discriminatedUnion("op", […])`) is a few kilobytes earlier, keyed
by `respawn-stale`.

## Addressing

- **Socket**: `<base>/cc-daemon-<uid>/<ns>/control.sock`, where `<ns>` is
  `sha256(path.resolve(configDir)).hex[:8]` and configDir is
  `$CLAUDE_CONFIG_DIR` or `~/.claude`. `<base>` is `/tmp`, except under
  Termux (`TERMUX_VERSION` and `PREFIX` both set) where it is `$PREFIX/tmp`.
  `$TMPDIR` is never consulted. Hive's `_daemon_control_sock` hardcodes
  `/tmp`, correct everywhere but Termux, which hive does not target. The
  namespace directory also holds `pty/`, `rv/` and `spare/`, the two live
  reply transports and the spare pool.
- **configDir**: the binary never reads `CLAUDE_HOME` (zero occurrences in
  2.1.240); hive's `_config_dir` prefers it over `CLAUDE_CONFIG_DIR`. A dev
  lane that sets only `CLAUDE_HOME` makes hive hash a directory the daemon has
  never heard of, so `daemon_reply` finds no socket and every delivery quietly
  takes the wrapped inbox lane. Set `CLAUDE_CONFIG_DIR` too when sandboxing.
- **Windows**: a named pipe `\\.\pipe\cc-daemon-<key>-control`, where `<key>`
  is 16 hex from `<configDir>/daemon/pipe.key`, not the configDir hash.
  Nothing in hive implements this.
- **`short`**: the job address, `^[a-f0-9]{8}$` (a non-matching string is a
  schema failure, not `ENOJOB`). It is the first 8 chars of the job's session
  UUID, and it is also the jobId: the dispatcher mints
  `sessionId = randomUUID()`, `short = sessionId.slice(0,8)`, and returns
  `{jobId: short, sessionId}`. It is likewise the directory name under
  `~/.claude/jobs/<short>/` and the `daemonShort` field of that job's
  `state.json`. Verified on all three live bg jobs: registry `jobId` equals
  `sessionId[:8]` for each.

## Framing

One JSON object per line in, one JSON line back (`{ok:true, op, …}` or
`{ok:false, error, code}`), then the daemon ends the connection. Three verbs
instead hold it open: `subscribe` and `attach` stream frames, and `lease`
registers the connection itself as the lease and releases on close.

Connection limits, all pre-parse:

- The peer uid must equal the daemon's. A mismatch answers
  `{code:"EPEERUID"}` — "permission denied: connecting uid X != daemon uid Y
  (retry without sudo, or as the daemon owner)" — and destroys the socket.
  Together with the 0700 socket directory this is the only guard on every
  verb that takes no `auth`.
- 30s to send the first line, then the connection is destroyed.
- 1 MiB (`1048576`) per request line; over that, `ETOOLARGE` ("request
  exceeds 1MB — shorten the prompt or send in parts"). A `reply` whose text
  crosses that is refused, which for hive means a silent fall-through to the
  inbox lane.

## Proto

Every request carries `proto` (integer). 2.1.240 accepts `1` only: the
server's min and max are both 1. Out of range, non-integer, or absent →
`{code:"EPROTO", serverProto, serverVersion}`, "background service and CLI
versions differ; restart claude".

The gate runs before op discrimination, so `EPROTO` does not report whether
the op sent exists. Six verbs are answered before the gate and therefore
ignore `proto` entirely: `ping`, `nudge`, `yield`, `lease`, `leases`,
`shutdown`. That makes `ping` the version probe that survives a skew: it
answers `{version, proto}` under any claimed `proto`, where any other verb
under a bad `proto` answers only `EPROTO`. Those same six also skip the
adoption gate (`ESTARTING`).

## Auth

`auth` is the contents of `<configDir>/daemon/control.key`: 16 random bytes
hex-encoded (32 chars), file mode 0600 inside a 0700 `daemon/` dir, minted on
first need and reused. Comparison is `timingSafeEqual` after a length check;
absent or empty never matches. The verb table below records which verbs demand
it. `attach` differs: an absent key is allowed (logged "[bg-attach] legacy
client (no control key) — allowed via peerUid"), a wrong key is rejected.

That table leaves `kill` and `shutdown` unauthenticated. Any process running
as the same uid can retire a worker or stop the supervisor and reap every bg
job with one JSON line.

## Error vocabulary

| code | meaning | reachable from |
| --- | --- | --- |
| `EPEERUID` | connecting uid ≠ daemon uid | any verb, pre-parse |
| `ETOOLARGE` | request line over 1 MiB | any verb, pre-parse |
| `EUNKNOWN` | bad JSON, unknown op, or schema failure | any verb |
| `EPROTO` | proto outside the server's range | every verb except the six pre-gate ones |
| `ESTARTING` | daemon up, worker adoption still in progress | every verb except the six pre-gate ones |
| `EAUTH` | control key absent or mismatched | `dispatch`, `reply`, `permission-response`, `attach` |
| `ENOJOB` | short unknown, retiring, killing, or already settled | `reply`, `kill`, `respawn-stale`, `resize`, `attach`, `subscribe` |
| `ERESPAWNING` | worker restarting (respawn set, or `isUpgrading`) | `reply`, `attach` |
| `ENOREPLY` | worker alive but its reply channel refused the text | `reply` |
| `EUNVERIFIED` | worker live but the supervisor could not verify its identity | `attach` |
| `ESTALE` | a previous dispatch with this id is still being cleaned up | `dispatch`, `await-ack` |
| `ETIMEOUT` | the worker did not acknowledge within `timeoutMs` | `dispatch`, `await-ack` |

Those twelve plus `EALIVE`, enumerated in the daemon's telemetry map but
returned by no verb surveyed, are the whole vocabulary. `ENOTOWNED` is a
bind-time failure, "refusing to bind: <dir> is owned by uid N", not a
response.

`EUNKNOWN` covers three different failures with two different texts: `"bad
json"`, `"unknown op: <op>"`, and `"malformed request: <first zod issue>"`.
A known op missing a required field is the third: `{"op":"has"}` answers
`"malformed request: Invalid input: expected string, received undefined"`,
while an unknown op answers the bare `"Invalid input"` (the union
discriminant failing). That difference separates an existing verb from an
unknown one without executing it, but only for verbs that have a required
field. `ping`, `nudge`, `yield`, `lease`, `leases` and `shutdown` need nothing
but `op`, so a bare probe runs them: `{"op":"shutdown"}` stops the supervisor
and reaps every worker.

## Verbs (2.1.240)

Input is the zod schema verbatim; `?` marks optional.

| op | auth | input | returns |
| --- | --- | --- | --- |
| `ping` | no | — | `{version, proto}` |
| `nudge` | no | — | `{restarting, version, processWrapper}` |
| `yield` | no | — | `{yielding}` |
| `lease` | no | `client?:{label,cwd,pid}` | `{ok:true}`, connection held open |
| `leases` | no | — | `{clients:[…]}` |
| `list` | no | — | `{jobs:[record…]}` |
| `has` | no | `short` | `{alive, present, ready}` |
| `reply` | **yes** | `short`, `text` | `{ok:true, op:"reply"}` |
| `dispatch` | **yes** | `d` (dispatch record), `timeoutMs` | `{short, pid, messagingSock, via}` |
| `await-ack` | no | `short`, `nonce?`, `timeoutMs` | same shape as `dispatch` |
| `kill` | no | `short`, `signal?:SIGTERM\|SIGKILL`, `handoff?`, `evict?` | `{ok:true}` |
| `respawn-stale` | no | `short` | `{ok:true, …respawn result}` |
| `resize` | no | `short`, `cols`, `rows`, `attachId?` | `{ok:true}` |
| `attach` | optional | `short`, `cols`, `rows`, `attachId?`, `caps?`, `holdingFrame?` | `{imarkNonce, decModes, via, booting, tempo, state, cached, stale}` then the pty stream |
| `subscribe` | no | `short`, `tail?` | streaming frames |
| `ensure-spare` | no | `cwd` | `{ok:true}` — **stub** |
| `permission-response` | **yes** | `short`, `requestId`, `allow` | `{ok:true}` — **stub** |
| `shutdown` | no | `reapWorkers?` | `{reaped:N}` |

Behavior the names do not describe:

- **`list`** maps the supervisor's in-memory worker handles, adding
  `dying:true` for one that is killing or retiring; it is not the job ledger.
  Parked and stopped jobs are simply absent: probed live at 3 records against
  52 directories under `~/.claude/jobs/`.
- **`has`** cannot see a parked job either. `alive` = a live handle (or the
  short sitting in the respawn set), `present` = a handle exists at all,
  `ready` = present and not booting. A stopped job whose directory and
  `state.json` are still on disk answers
  `{"alive":false,"present":false,"ready":false}`, indistinguishable from a
  short that never existed. Probed against a real `state: stopped` job.
- **`ensure-spare` and `permission-response`** answer `ok:true` and do
  nothing. `ensure-spare` prewarms nothing: it fires the
  `tengu_dead_probe_bg_legacy_op` telemetry event and returns, ignoring the
  `cwd` it demands. `permission-response` checks the control key and returns,
  ignoring `short`, `requestId` and `allow`; there is no plumbing behind it
  on 2.1.240.
- **`respawn-stale`** fires the same dead-op probe but still does the work
  (`respawnIfIdleStale()`). The probe is upstream's instrumentation for
  removal candidates, so the op is live but deprecated.
- **`kill`**: with `evict:true` the roster row is deleted before the short is
  validated, so an unknown short still loses its row and then gets `ENOJOB`.
- **`subscribe` frames**: `{type:"snapshot", record, streamTail}` (tail
  defaults to 200 lines), then `{type:"stream", line}` and
  `{type:"state", patch}` until `{type:"settled", outcome}` closes the
  connection. A job already settled gets snapshot + settled and nothing else.
- **`dispatch` and `await-ack`** share one rendezvous loop. It polls every
  25ms until `min(timeoutMs, 30000)`, and when a `nonce` is supplied it waits
  for the handle whose `record.nonce` matches, extending the budget once.

Hive uses `attach` only through the published CLI (`claude attach <jobId>` as
a subprocess), not over this socket.

## `reply`: the typed-keystroke delivery lane

`reply` reaches the worker's `reply()`, which is three branches:

1. The job's `state.json` is unreadable, or its `tempo` is `blocked`
   (the enum is `active | idle | blocked`) → the text goes over the worker's
   rv channel as `{type:"reply", text}`. On the worker side that either
   answers the pending question outright, or enqueues
   `{mode, value, priority:"next", origin:{kind:"human"}}`.
2. The worker has a **pty** → bracketed paste (`ESC[200~ text ESC[201~`,
   raw for an `exec`-mode job) followed by `\r` 10ms later, serialized
   through the worker's `replyChain`. This is the official version of what
   `claude_bg::type_into_job` hand-rolls at the pane.
3. **No pty** → the rv channel again; a channel that refuses is `ENOREPLY`.

Two consequences follow. `origin.kind === "human"` is what the receiving side keys the peer wrapper
off: the wrapper is applied for `peer`, `channel`, `observer`, `slack-ping`
and `unclassified`, and skipped for `human`, `auto-continuation`,
`task-notification` and undefined. A `reply`-delivered message therefore
carries no banner in any layer, which is why it, not the inbox socket, is
hive's primary claude-member lane. The wrapped inbox socket is the fallback
when this lane returns nothing, so an outage here degrades presentation
rather than delivery; hive drives an unpublished surface on the strength of
that fallback.

`{ok:true}` on branch 2 means the bytes were queued into the pty and nothing
more: the branch returns as soon as the write is chained, before the worker
has read a keystroke. Only branch 3, and a branch 1 whose rv channel refuses,
can produce `ENOREPLY`. A `reply` acceptance is evidence that the composer was
written to, not that a turn started; the response obligation still rides the
member skill's receipt duty.

Two notes on hive's client (`claude_sessions::daemon_reply_via`), which the
code does not say:

- The CLI's own attach client treats `ESTARTING`/`ERESPAWNING` as transient
  and boots a daemon on a dead socket. Hive folds all three retry codes into
  one budget and deliberately boots nothing: the fallback lane already covers
  a supervisor that is not running.
- The `EAUTH` re-read is one retry, not a loop, because the daemon does not
  rotate the key in normal operation: it is minted once and reused. A second
  `EAUTH` means the key is wrong, not stale.

## Tech-debt offset candidates (not adopted, recorded)

Per the "mechanism GO ≠ integrate" rule, each needs a named consumer plus a
measured current cost before adoption.

- `list` / `has` — ruled out for liveness. Hive's three-tier model
  (alive / asleep / gone) turns on separating a parked job from a removed
  one, which neither verb can do. `claude agents --json --all` stays the only
  source for the asleep tier, at ~270ms a call. `list` is still the cheaper
  answer for "which workers are live right now", if a consumer ever wants
  only that.
- `kill` — member reap could retire workers through the supervisor
  (`evict:true` also drops the roster row) instead of signalling pids.
- `respawn-stale` — a stuck-member self-heal lever the wake path does not
  have. Deprecation-flagged upstream: re-check the next version before
  building on it.
- `dispatch` / `await-ack` — a drop-in for `claude_bg::spawn_job`'s
  subprocess-plus-stdout-regex minting, and it returns
  `{short, pid, messagingSock}` directly, which is what spawn then goes
  looking for in the registry. The record `d` is fully schema'd, so the port
  is mechanical:

  ```
  { proto, short, nonce?, sessionId, createdAt, cwd,
    source: shell|slash|fleet|spare|respawn,
    launch: { mode:"prompt", args[], restoresTranscript? }
          | { mode:"resume", sessionId, transcriptPath?, fork, flagArgs[], … }
          | { mode:"exec", cmd, args[] },
    env{}, reattachEnv?{}, worktree?{path,ownershipToken},
    isolation: none|worktree, respawnFlags[], attachStallRespawns?,
    agent?, routine?, seed?{intent,name?}, cols?, rows? }
  ```

  Still not adopted: it trades the published `--bg` CLI contract for an
  unpublished wire record, it has never been live-tested, and the one cost of
  the current path (a FORCE_COLOR-poisoned jobId parse) is already fixed and
  guarded.
- `subscribe` — push state and stream instead of polling. Surveyed and
  retracted: no consumer beat the sub-millisecond registry scan it would
  replace.
