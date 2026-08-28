# The claude supervisor daemon's control socket

Binary-extracted survey of the bg supervisor daemon's control protocol,
observed on Claude Code 2.1.240 (strings-level source recovery, verbs
live-verified where noted). This is an unpublished surface: every claim here
is versioned to 2.1.240 and must be re-verified on upgrade. Hive consumes
`reply` (delivery lane, `adapters/claude_sessions.daemon_reply`); the rest is
recorded because several verbs can replace machinery hive currently
hand-rolls.

## Addressing

- **Socket**: `/tmp/cc-daemon-<uid>/<ns>/control.sock` where
  `ns = sha256(resolve(configDir))[:8]` and configDir is `$CLAUDE_CONFIG_DIR`
  or `~/.claude`. `/tmp` is fixed — the daemon ignores `$TMPDIR` (Termux uses
  `$PREFIX/tmp`). Windows uses a named pipe (`\\.\pipe\cc-daemon-…`) keyed by
  `<configDir>/daemon/pipe.key`.
- **Framing**: one JSON object per line in; one JSON line back
  (`{ok, op, …}` or `{ok:false, error, code}`). `subscribe` and `attach`
  instead keep the connection open and stream frames.
- **Proto**: every request carries `proto` (integer; `1` on 2.1.240). Out of
  range → `EPROTO` with the server's version — the "restart claude" skew
  guard.
- **Auth**: mutating verbs (`dispatch`, `reply`, `attach`, in part) require
  `auth` = contents of `<configDir>/daemon/control.key` (0600, 32 hex,
  minted on demand). Wrong/absent key → `EAUTH`. Read verbs (`list`, `has`,
  `subscribe`) take no auth on 2.1.240.
- **`short`**: job address = first 8 hex of its session id (also the
  directory name under `~/.claude/jobs/`).

## Error vocabulary

| code | meaning | client reaction (binary's own) |
| --- | --- | --- |
| `ESTARTING` | daemon up, adoption in progress | retry, 200ms backoff |
| `ERESPAWNING` | worker restarting (e.g. across an update) | retry, longer budget |
| `ENOREPLY` | worker alive but not accepting input (non-interactive state) | retry |
| `ENOJOB` | short unknown / already exited | terminal |
| `EAUTH` | control key mismatch | re-read key once, retry |
| `EPROTO` | proto out of server range | terminal ("restart claude") |
| `EUNKNOWN` | malformed request / unknown op | terminal |

## Verbs (2.1.240)

| op | auth | input | returns | notes |
| --- | --- | --- | --- | --- |
| `ping` | no | — | `{ok:true}` | liveness |
| `list` | no | — | `{jobs:[record…]}` | full job records in one call; dying workers flagged `dying:true` |
| `has` | no | `short` | `{alive, present, ready}` | three-valued liveness probe |
| `reply` | yes | `short`, `text` | `{ok:true, op:"reply"}` | **the human input lane** — see below. Live-verified (idle / mid-turn / blocked, all clean) |
| `dispatch` | yes | `d` (dispatch record), `timeoutMs` | ack via await machinery | mint a new bg job directly |
| `await-ack` | no | `short`, `nonce`, `timeoutMs` | ack when worker reaches the nonce | dispatch/ack rendezvous |
| `kill` | no | `short`, `signal?`, `handoff?`, `evict?` | `{ok:true}` | supervisor-side kill; `evict` also drops the roster row |
| `respawn-stale` | no | `short` | `{ok:true, …}` | revive an idle-stale worker in place |
| `resize` | no | `short`, `cols`, `rows`, `attachId?` | `{ok:true}` | pty winsize; per-attacher when `attachId` given |
| `attach` | yes* | `short`, … | streaming | the pty stream `claude attach` rides; legacy no-key clients allowed via peer uid |
| `subscribe` | no | `short`, `tail?` | streaming: `snapshot` → `state` patches / `stream` lines → `settled` | push observability (surveyed 2026-08-27; integration retracted — no consumer beat the 0.4ms registry scan) |
| `ensure-spare` | no | — | `{ok:true}` | prewarm a spare worker |
| `permission-response` | yes | … | `{ok:true}` | permission-prompt answer plumbing |
| `nudge` / `yield` / `lease` / `leases` / `shutdown` | no | — | bookkeeping | client lease/lifecycle chores |

## `reply`: the typed-keystroke delivery lane

`reply` routes to the worker's own reply channel, three branches, every one
of them the human lane:

- worker **blocked** (permission prompt / question open) → the text goes over
  the worker's rv channel; the engine either answers the pending question
  with it or enqueues it `origin:{kind:"human"}, priority:"next"`.
- worker normal with a **pty** → bracketed-paste + Enter into the pty,
  serialized through the worker's replyChain (the official version of what
  `type_into_job` hand-rolls).
- **no pty** → rv channel, same human-origin enqueue.

Consequence: a `reply`-delivered message never carries the peer wrapper in
any layer — idle arrival starts its own turn (mechanical response
guarantee), mid-turn arrival folds in at the next tool boundary as a bare
`❯` line. This is hive's primary claude-member delivery lane
(`daemon_reply`); the inbox socket (`send`, `priority:"next"`, wrapped) is
the fallback when the daemon is unreachable. The response obligation for
folded arrivals rides the member skill's receipt duty, not the carriage.

## Tech-debt offset candidates (not adopted, recorded)

Per the "mechanism GO ≠ integrate" rule, each needs a named consumer plus a
measured current cost before adoption:

- `list` / `has` — hive's job-ledger file scans and liveness heuristics
  (`claude_bg.job_row`, reaper checks) could ask the supervisor instead of
  inferring from files.
- `kill` — member reap could retire workers through the supervisor
  (`evict:true` cleans the roster) instead of signalling pids.
- `respawn-stale` — a stuck-member self-heal lever the wake path doesn't
  have today.
- `dispatch` / `await-ack` — a drop-in for `claude_bg.spawn_job`'s
  subprocess-plus-stdout-regex minting (the pane launcher is already just an
  attach viewer). Not adopted: it trades the published `--bg` CLI contract
  for an unpublished wire record (`d`, shape unknown, never live-tested),
  and the one real cost of the current path (FORCE_COLOR-poisoned jobId
  parse) is already fixed and tested.
- `subscribe` — push state/stream; already surveyed and retracted for lack
  of a consumer (see reports/wrapped-verdict.html era notes).
