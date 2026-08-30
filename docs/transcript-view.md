# The Transcript Mirror (`hive view`)

## Why It Exists

An interactive Claude session — a desktop ccd, a session collected by `hive
join` — has no attachable pty. `claude attach` addresses bg jobs only, and
`--resume` does not attach to the running engine: it forks a second one,
which mints a bg job that then steals the member's deliveries. The only
remaining surface is the transcript JSONL, which claude appends event by
event as the turn unfolds.

So `hive attach` substitutes the viewer for any claude member with no bg-job
row.

## The Boundary

Input: one transcript JSONL, nothing else. Output: one setting, `view.theme`,
written when `/theme` is used. No socket, no pty, no registry, no bus, no
hived. Keystrokes reach the session by no path at all — a property of the
process rather than a policy it enforces, which is what makes the mirror safe
to point at a live session someone else is driving. How a message got into
that session is [runtime-model.md](runtime-model.md)'s subject, not this
one's.

The consequence worth internalizing: **the Claude Code UI is not a mirror of
the transcript, so this is not a mirror of the UI.** The JSONL records what
was committed. The UI additionally holds live uncommitted state that never
becomes a row — most visibly, a message sitting in the input queue is on the
user's screen, is not here, and may be cancelled before it ever becomes one.

## What The Transcript Actually Contains

The parse layer is written against the format as observed; claude documents
none of it. The facts below come from scanning the ~2,200 transcripts under
`~/.claude/projects` (2026-08, claude 2.1.2xx). They are observations, not a
contract — re-check them after a claude upgrade before trusting a parser
change.

**Row types far outnumber the ones that render.** Besides `user` /
`assistant` / `attachment` / `queue-operation`, real sessions write `system`,
`last-prompt`, `custom-title`, `mode`, `atis-latch`, `pr-link`,
`file-history-snapshot` and more; the `attachment` family is larger still.
Everything unrecognized is dropped by design — the mirror renders the
conversation, not the session bookkeeping around it.

**A HIVE message arrives in five carriers, and the fifth leaves no `user`
row.** Bare; wrapped in claude's peer-message injection at turn start; the
same wrapper folded into a running turn; the retired `<channel>` block, still
present in old transcripts; and — when it arrived mid-turn — recorded *only*
as a `queued_command` attachment plus `queue-operation` rows. A viewer that
reads `user` rows alone silently loses the fifth, which is exactly the case
that occurs whenever a peer messages an agent already working.

**`queue-operation` is an append-only log with four verbs**: `enqueue`,
`dequeue`, `remove` (with and without `reason`), `popAll`. Only two states
are terminal, and both mean the model saw the message — `dequeue`, after
which a `user` row follows, and `remove` with `reason: absorbed_mid_turn`,
after which none ever will. `enqueue` draws nothing because the message can
still be cancelled. Absorption usually also writes the richer
`queued_command` attachment, the only record carrying the sender's origin;
about 5% of absorbed rows carry no `content` field and are drawable only from
the attachment beside them — which in every observed case exists and carries
the words, in an array prompt the parser refuses. Hence two draw paths and a
dedupe set keyed on the raw text.

**The `queued_command` row is backdated.** Its `timestamp` is when the
message was accepted, not when the row was appended: 92% of them read earlier
than the row physically above them, by seconds. Blocks are placed at file
position, so a mid-turn message's clock can read earlier than the block above
it on screen. The one thing this would corrupt is the thinking-duration
anchor, which is computed from adjacent row timestamps; attachment rows
return before the anchor advances.

**11% of `queued_command` prompts are not strings.** When the mid-turn
message carried an image the prompt is an array, and the parser requires a
string, so those messages never appear at all.

**Thinking text is unreliable.** A `thinking` content block often arrives
carrying a `signature` and an empty `thinking` string. This is not a version
or model cutover: across the corpus roughly half the transcripts persist the
text, half persist nothing, and 255 files flip mid-session. The header
renders either way — its duration is computed from adjacent row timestamps,
never from the model — so expanding a thinking block is a coin flip between
prose and `(no content)`. That is why the density ladder lost its middle
rung: a level whose whole purpose is expanding thinking is worthless when the
content is missing half the time.

**Image payloads are in the file and are never read.** A pasted screenshot is
a few hundred KB of base64; rendered, thousands of wrapped scrollback lines.
In user rows and inside `tool_result` arrays alike it is replaced by a
numbered chip that keeps its position among the words it arrived with.

**Ultra effort has no field.** Assistant rows carry an `effort` and it never
says `ultra`; entering ultra writes one `ultra_effort_enter` attachment,
once, usually far above the tail window — which is why the whole prelude is
scanned for it before the first draw, and it is the only thing recovered from
above the window. Real transcripts also record `ultra_effort_exit`. Nothing
reads that row, so the badge latches: a session that dropped out of ultra
still badges as ultra.

## Deliberate Blindness

**A mid-turn message that is neither a HIVE envelope nor `origin.kind:
"human"` never renders.** That is where runtime plumbing such as task
notifications lives, and keeping it off the screen is the point. Accepted
cost: a peer message in some future non-HIVE shape would vanish without a
trace.

**The window is a tail, not an archive.** Only the last couple hundred rows
of the backlog are rendered; everything above is read once and discarded,
save the ultra scan. Scrolling up reaches the top of that window, not the top
of the session. The mirror is for watching a session, not auditing one — the
JSONL is right there for that.

## What It Borrows From Grok

The look and most of the interaction semantics come from Grok Build's pager,
in three different ways, and the difference decides how a change is made.

**Linked**: `xai-grok-markdown` at a pinned rev, driving every assistant and
thinking body, and `tui-scrollbar`, the same crates.io scrollbar grok itself
uses.

**Vendored**: `grok-night.tmTheme` and `grok-day.tmTheme`, byte-identical to
the pager's assets at that rev, embedded with `include_bytes!`. The rev pin
is what keeps syntect's code-fence colors matching the two palettes, which
were transcribed by hand from grok's theme structs — move one without the
other and code fences drift out of the theme.

**Reimplemented**: selection, turn jumps, tool blocks, the thinking blend,
the block viewer, the turn-status row, the mouse constants. Each hive-side
component names its grok source in a doc comment; the full source sits in the
cargo checkout (path in `AGENTS.md`). Read it before changing a mirrored
component — the constants that look arbitrary are grok's, and were chosen
against grok's layout.

Every divergence is downstream of two facts: this surface is read-only, and
it lives in tmux. The composer never accepts text and the turn-status row has
no `[stop]`. The OSC 11 probe skips grok's DCS-passthrough retry because
tmux ≥ 3.2 answers the bare query from inside a pane. Auto is the default
appearance and detection failure falls light, where grok defaults dark and
falls dark. Density is hive's own idea with no grok counterpart.
