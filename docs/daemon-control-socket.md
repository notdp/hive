# The claude supervisor daemon's control socket

The Claude Code bg supervisor's control protocol, recovered from the shipped
binary and probed live. This is an unpublished surface: every claim here is
versioned, and the daemon's own `EAUTH` text tells clients to "stop driving
the control socket directly". Hive drives exactly one verb — `reply`, the
claude-member delivery lane (`crates/hive/src/adapters/claude_sessions.rs:478`);
the rest is recorded because several verbs look like replacements for
machinery hive hand-rolls, and because knowing what they actually do is what
rules most of them out.

## The pin

Pinned to **2.1.240**, and that still matches what is installed:

```
$ claude --version
2.1.240 (Claude Code)
```

and writing `{"proto":1,"op":"ping"}` plus a newline to the socket answers
`{"ok":true,"op":"ping","version":"2.1.240","proto":1}` — the running
supervisor agrees with the installed CLI.

The binary is `~/.local/share/claude/versions/2.1.240` (Mach-O, bun-compiled;
the JS bundle sits in it as plain text). Build metadata it carries:
`BUILD_TIME 2026-08-22T05:07:39Z`, `GIT_SHA d235569e3f61fc4d9aacd7c85e6d1b6253e03f52`.

To re-recover after an upgrade, find the request handler by a stable literal
rather than a minified name:

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
  `$TMPDIR` is never consulted. Hive computes the same path but hardcodes
  `/tmp` (`claude_sessions.rs:437`) — correct everywhere but Termux.
- Verified locally: `sha256("/Users/<u>/.claude")[:8] == 758434e4`, and
  `/tmp/cc-daemon-501/758434e4/` holds `control.sock` plus `pty/`, `rv/`,
  `spare/`.
- **Windows** uses a named pipe `\\.\pipe\cc-daemon-<key>-control`, where
  `<key>` is 16 hex from `<configDir>/daemon/pipe.key` — not the configDir
  hash.
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
  Together with the 0700 socket directory this is the *only* guard on every
  verb that takes no `auth`.
- 30s to send the first line, then the connection is destroyed.
- 1 MiB (`1048576`) per request line; over that, `ETOOLARGE` ("request
  exceeds 1MB — shorten the prompt or send in parts"). A `reply` whose text
  crosses that is refused, which for hive means a silent fall-through to the
  inbox lane.

## Proto

Every request carries `proto` (integer). 2.1.240 accepts `1` only — the
server's min and max are both 1. Out of range, non-integer, or absent →
`{code:"EPROTO", serverProto, serverVersion}`, "background service and CLI
versions differ; restart claude".

The gate runs **before** op discrimination, so `EPROTO` never tells you
whether the op you sent exists. Six verbs are answered before the gate and
therefore ignore `proto` entirely: `ping`, `nudge`, `yield`, `lease`,
`leases`, `shutdown`. That makes `ping` the version probe that survives a
skew — it answers `{version, proto}` whatever you claim:

```
$ … '{"proto":99,"op":"ping"}'  → {"ok":true,"op":"ping","version":"2.1.240","proto":1}
$ … '{"proto":99,"op":"has","short":"deadbeef"}'
  → {"ok":false,"code":"EPROTO","serverProto":1,"serverVersion":"2.1.240", …}
```

Those same six also skip the adoption gate below.

## Auth

`auth` is the contents of `<configDir>/daemon/control.key`: 16 random bytes
hex-encoded (32 chars), file mode 0600 inside a 0700 `daemon/` dir, minted on
first need and reused. Comparison is `timingSafeEqual` after a length check;
absent or empty never matches.

- **Required**: `dispatch`, `reply`, `permission-response`. Wrong or missing
  → `EAUTH`.
- **Optional-but-checked**: `attach`. Absent is allowed (logged
  "[bg-attach] legacy client (no control key) — allowed via peerUid"); a
  *wrong* key is rejected.
- **None**: `ping`, `nudge`, `yield`, `lease`, `leases`, `shutdown`, `list`,
  `has`, `await-ack`, `kill`, `respawn-stale`, `resize`, `ensure-spare`,
  `subscribe`.

That last line is the sharp edge: `kill` and `shutdown` are unauthenticated.
Any process running as the same uid can retire a worker or stop the
supervisor and reap every bg job with one JSON line.

## Error vocabulary

The daemon's full code set, as its own telemetry map enumerates it:
`ENOJOB`, `ETIMEOUT`, `EUNKNOWN`, `ENOREPLY`, `ERESPAWNING`, `ESTALE`,
`EALIVE`, `ESTARTING`, `EPEERUID`, `ETOOLARGE`, `EUNVERIFIED`, `EAUTH`,
`EPROTO`. (`ENOTOWNED` is a bind-time failure — "refusing to bind: <dir> is
owned by uid N" — never a response.)

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

`EUNKNOWN` covers three different failures with two different texts: `"bad
json"`, `"unknown op: <op>"`, and `"malformed request: <first zod issue>"`.
A known op missing a required field is the third — `{"op":"has"}` answers
`"malformed request: Invalid input: expected string, received undefined"`,
while an unknown op answers the bare `"Invalid input"` (the union
discriminant failing). That difference is the only safe way to probe whether
a verb exists without executing it.

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

Details that matter:

- **`list` is not the job ledger.** It maps the supervisor's in-memory worker
  handles, adding `dying:true` for one that is killing or retiring. Live
  here: 3 records, against 52 directories under `~/.claude/jobs/` — the
  parked and stopped jobs are simply absent. Record fields observed:
  `attempt`, `backend`, `cliVersion`, `createdAt`, `cwd`, `detail`, `intent`,
  `name`, `nonce`, `pid`, `sessionId`, `short`, `source`, `startedAt`,
  `state`, `tempo`.
- **`has` cannot see a parked job either.** `alive` = a live handle (or the
  short sitting in the respawn set), `present` = a handle exists at all,
  `ready` = present and not booting. A stopped job whose directory and
  `state.json` are still on disk answers
  `{"alive":false,"present":false,"ready":false}` — indistinguishable from a
  short that never existed. Probed against a real `state: stopped` job.
- **`ensure-spare` prewarms nothing.** The handler fires the
  `tengu_dead_probe_bg_legacy_op` telemetry event and returns `{ok:true}`,
  ignoring the `cwd` it demands. It is instrumented as a candidate for
  removal upstream.
- **`permission-response` answers nothing.** It checks the control key and
  returns `{ok:true}`, ignoring `short`, `requestId` and `allow`. There is no
  plumbing behind it on 2.1.240.
- **`respawn-stale` fires the same dead-op probe** but still does the work
  (`respawnIfIdleStale()`). Treat it as live-but-deprecated.
- **`kill` evicts before it checks.** With `evict:true` the roster row is
  deleted first, so an unknown short still loses its row and *then* gets
  `ENOJOB`.
- **`subscribe` frames**: `{type:"snapshot", record, streamTail}` (tail
  defaults to 200 lines), then `{type:"stream", line}` and
  `{type:"state", patch}` until `{type:"settled", outcome}` closes the
  connection. A job already settled gets snapshot + settled and nothing else.
- **`dispatch` and `await-ack` share one rendezvous loop.** It polls every
  25ms until `min(timeoutMs, 30000)`, and when a `nonce` is supplied it waits
  for the handle whose `record.nonce` matches, extending the budget once.

Hive touches `attach` only through the CLI — `claude attach <jobId>` as a
subprocess (`crates/hive/src/adapters/claude_bg.rs:1336`) — never over this
socket.

## `reply`: the typed-keystroke delivery lane

`reply` reaches the worker's `reply()`, which is three branches:

1. The job's `state.json` is unreadable, **or** its `tempo` is `blocked`
   (the enum is `active | idle | blocked`) → the text goes over the worker's
   rv channel as `{type:"reply", text}`. On the worker side that either
   answers the pending question outright, or enqueues
   `{mode, value, priority:"next", origin:{kind:"human"}}`.
2. The worker has a **pty** → bracketed paste (`ESC[200~ text ESC[201~`,
   raw for an `exec`-mode job) followed by `\r` 10ms later, serialized
   through the worker's `replyChain`. This is the official version of what
   `claude_bg.type_into_job` hand-rolls at the pane.
3. **No pty** → the rv channel again; a channel that refuses is `ENOREPLY`.

Two consequences.

`origin.kind === "human"` is what the receiving side keys the peer wrapper
off: the wrapper is applied for `peer`, `channel`, `observer`, `slack-ping`
and `unclassified`, and skipped for `human`, `auto-continuation`,
`task-notification` and undefined. So a `reply`-delivered message carries no
banner in any layer — which is why this, not the inbox socket, is hive's
primary claude-member lane (`crates/hive/src/agent.rs:813`, and
`agent.rs:844` for a joined `ccd.<name>` session). The wrapped inbox socket
is the fallback when this lane returns nothing.

And `{ok:true}` on branch 2 means *written into the pty*, not *accepted as a
turn*: the branch resolves as soon as the bytes are queued on `replyChain`.
Only branch 3 (and a failing branch 1) can report refusal. A delivery
confirmed here is a delivery the composer received.

## Hive's client

`claude_sessions.rs:490` `daemon_reply_via` — one frame,
`{proto:1, op:"reply", short, auth, text}`, no `session_id` field, a fresh
connection per attempt with 10s read and write timeouts:

- `short` is `session_id[:8]`, taken without checking it is hex
  (`claude_sessions.rs:496`). A non-hex prefix comes back `EUNKNOWN`, which
  is terminal — the caller falls back to the inbox lane rather than looping.
- `ok:true` → `daemonReplyAccepted`.
- `EAUTH` → re-read the key once and retry with it; a second `EAUTH` is
  terminal. The daemon does not rotate the key in normal operation, so this
  costs one wasted round trip at most.
- `ESTARTING`, `ENOREPLY`, `ERESPAWNING` → sleep 200ms and retry, up to 24
  attempts (`claude_sessions.rs:36-39`). All three share one budget, unlike
  the CLI's own attach client, which retries `ESTARTING`/`ERESPAWNING` as
  transient and separately boots a daemon on a dead socket.
  `SUBMIT_TIMEOUT` folds that 4.8s into the connect and write budgets so a
  hived RPC cannot outlive its own retry run (`claude_sessions.rs:42`).
- Anything else — `ENOJOB`, `EPROTO`, `EUNKNOWN`, `ETOOLARGE`, `EPEERUID`,
  no socket at all — is `None`, and delivery falls through to the inbox
  socket, which still delivers (wrapped).

## Tech-debt offset candidates (not adopted, recorded)

Per the "mechanism GO ≠ integrate" rule, each needs a named consumer plus a
measured current cost before adoption.

- `list` / `has` — **ruled out for liveness.** Hive's three-tier model
  (alive / asleep / gone) turns on separating a parked job from a removed
  one, and neither verb can: both report a parked job exactly as they report
  a job that never existed. `claude agents --json --all` stays the only
  source for the asleep tier (`claude_bg.rs:482` `job_row`, ~270ms, cached).
  `list` is still the cheaper answer for "which workers are live right now",
  if a consumer ever wants only that.
- `kill` — member reap could retire workers through the supervisor
  (`evict:true` also drops the roster row) instead of signalling pids. No
  auth needed. The evict-before-check ordering means a stale short still
  cleans its row.
- `respawn-stale` — a stuck-member self-heal lever the wake path does not
  have. Deprecation-flagged upstream; do not build on it without re-checking
  the next version.
- `dispatch` / `await-ack` — a drop-in for `claude_bg.spawn_job`'s
  subprocess-plus-stdout-regex minting (`claude_bg.rs:532`), and it returns
  `{short, pid, messagingSock}` directly, which is what spawn then goes
  looking for in the registry. The record `d` is fully schema'd:

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
  unpublished wire record, it has never been live-tested, and the one real
  cost of the current path (a FORCE_COLOR-poisoned jobId parse) is already
  fixed and guarded (`claude_bg.rs:556`).
- `subscribe` — push state and stream instead of polling. Surveyed and
  retracted: no consumer beat the sub-millisecond registry scan it would
  replace.
- `ensure-spare` / `permission-response` — not candidates. Both are stubs
  that answer `ok:true` and do nothing.
